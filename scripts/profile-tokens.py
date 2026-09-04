#!/usr/bin/env python3
"""Profile how many tokens kanban4ai injects into agent context windows.

Reads the board's own run history (`.kanban/logs/*.prompt.txt` plus the
matching `*.transcript.jsonl`) and reports, per backend and per task
lifecycle stage:

  * how many tokens the kanban-authored prompt itself costs, split into the
    fixed session contract, the role block, the user's task text and the
    replayed thread context;
  * how many tokens the backend adds on top (its own system prompt, tool
    schemas, AGENTS.md/CLAUDE.md), measured as turn-1 context minus the
    kanban prompt;
  * how the cost repeats across relaunches of the same task.

Token counts use tiktoken's cl100k_base when available. That is not any
vendor's exact tokenizer, but it is stable and within a few percent for
English prose, which is what the prompt is. Backend overhead numbers are not
estimated at all: they come from the `usage` objects the backends themselves
reported.

Usage:
    profile-tokens.py [--logs DIR] [--json] [--limit N]
"""

from __future__ import annotations

import argparse
import json
import os
import re
import statistics
import sys
from collections import defaultdict
from pathlib import Path

# --- tokenizer ------------------------------------------------------------

def _load_encoder():
    try:
        import tiktoken
    except ImportError:
        return None
    try:
        return tiktoken.get_encoding("cl100k_base")
    except Exception:
        return None


_ENC = _load_encoder()


def ntok(text: str) -> int:
    """Token count for `text`; falls back to a chars/4 estimate."""
    if not text:
        return 0
    if _ENC is None:
        return round(len(text) / 4)
    return len(_ENC.encode(text, disallowed_special=()))


# --- prompt decomposition -------------------------------------------------

# Markers emitted by src/agent/prompt.rs. Order matters: the prompt is built
# header -> contract -> role block -> isolation -> user task -> thread.
ROLE_MARKER = re.compile(r"\nRole: (executor|designer|reviewer)\nColumn ownership:\n")
ISOLATION_MARKER = "\nIsolation: you are working in an isolated git checkout"
TASK_MARKER = "\nUser task:\n"
THREAD_MARKER = "\n\nThread context and review feedback:\n"


def split_prompt(text: str) -> dict:
    """Split a built prompt into its cost segments.

    Returns a dict of segment name -> raw text. Segments that this prompt
    does not have come back as empty strings, so every prompt yields the
    same keys and the aggregate tables stay rectangular.
    """
    seg = {
        "header": "",
        "contract": "",
        "role_block": "",
        "isolation": "",
        "user_task": "",
        "thread": "",
    }

    rest = text
    # Thread context is the tail; take it off first so nothing else can
    # match inside replayed message bodies.
    idx = rest.find(THREAD_MARKER)
    if idx >= 0:
        seg["thread"] = rest[idx:]
        rest = rest[:idx]

    idx = rest.find(TASK_MARKER)
    if idx >= 0:
        seg["user_task"] = rest[idx + len(TASK_MARKER):]
        rest = rest[:idx]

    idx = rest.find(ISOLATION_MARKER)
    if idx >= 0:
        seg["isolation"] = rest[idx:]
        rest = rest[:idx]

    match = ROLE_MARKER.search(rest)
    if match:
        seg["role_block"] = rest[match.start():]
        rest = rest[:match.start()]

    idx = rest.find("Session contract:\n")
    if idx >= 0:
        seg["header"] = rest[:idx]
        seg["contract"] = rest[idx:]
    else:
        # Revert prompts have no contract section.
        seg["header"] = rest

    return seg


def detect_role(text: str) -> str:
    match = ROLE_MARKER.search(text)
    if match:
        return match.group(1)
    if "revert agent" in text[:400]:
        return "revert"
    return "unknown"


# --- transcript parsing ---------------------------------------------------

def _iter_json_lines(path: Path):
    try:
        with path.open("r", errors="replace") as handle:
            for line in handle:
                line = line.strip()
                if not line or not line.startswith("{"):
                    continue
                try:
                    yield json.loads(line)
                except json.JSONDecodeError:
                    continue
    except OSError:
        return


def parse_claude_transcript(path: Path) -> dict:
    """Turn-1 context size and run totals from a claude stream-json log.

    Turn-1 context is the whole prompt the model actually saw on its first
    call: uncached input + freshly cached input + cache hits. That is the
    number to compare against the kanban prompt, because everything else in
    it (system prompt, tool schemas, CLAUDE.md) is the backend's own tax.
    """
    out = {"turn1_context": None, "final_tokens": None, "cost_usd": None,
           "turns": 0, "model": None, "cache_read_total": 0}
    for event in _iter_json_lines(path):
        etype = event.get("type")
        if etype == "assistant":
            message = event.get("message") or {}
            usage = message.get("usage") or {}
            if not usage:
                continue
            out["turns"] += 1
            if out["model"] is None:
                out["model"] = message.get("model")
            context = (
                (usage.get("input_tokens") or 0)
                + (usage.get("cache_creation_input_tokens") or 0)
                + (usage.get("cache_read_input_tokens") or 0)
            )
            out["cache_read_total"] += usage.get("cache_read_input_tokens") or 0
            if out["turn1_context"] is None and context > 0:
                out["turn1_context"] = context
        elif etype == "result":
            usage = event.get("usage") or {}
            total = (usage.get("input_tokens") or 0) + (usage.get("output_tokens") or 0)
            if total:
                out["final_tokens"] = total
            if event.get("total_cost_usd") is not None:
                out["cost_usd"] = event.get("total_cost_usd")
    return out


def parse_codex_transcript(path: Path) -> dict:
    """Turn-1 context and totals from a Codex ``exec --json`` log.

    Codex reports cumulative usage on ``turn.completed``. Cached input is
    tracked separately because it is not fresh context, matching the Claude
    and pi-family reports above.
    """
    out = {"turn1_context": None, "final_tokens": None, "cost_usd": None,
           "turns": 0, "model": None, "cache_read_total": 0}
    for event in _iter_json_lines(path):
        if out["model"] is None:
            model = event.get("model")
            if isinstance(model, str):
                out["model"] = model
        if event.get("type") != "turn.completed":
            continue
        usage = event.get("usage") or {}
        input_tokens = usage.get("input_tokens") or 0
        cached = usage.get("cached_input_tokens") or 0
        output = usage.get("output_tokens") or 0
        context = input_tokens + cached
        if context <= 0:
            continue
        out["turns"] += 1
        out["cache_read_total"] += cached
        if out["turn1_context"] is None:
            out["turn1_context"] = context
        total = usage.get("total_tokens") or (input_tokens + output)
        if total:
            out["final_tokens"] = max(out["final_tokens"] or 0, total)
        cost = usage.get("cost_usd")
        if isinstance(cost, (int, float)):
            out["cost_usd"] = cost
    return out


def parse_pi_family_transcript(path: Path) -> dict:
    """Turn-1 context and totals from an omp/pi NDJSON log.

    Their `usage` uses {input, output, cacheRead, cacheWrite}; the first
    message that reports a non-zero input is the first real API call.
    """
    out = {"turn1_context": None, "final_tokens": None, "cost_usd": None,
           "turns": 0, "model": None, "cache_read_total": 0}
    for event in _iter_json_lines(path):
        if event.get("type") not in ("message_end", "turn_end"):
            continue
        message = event.get("message") or {}
        usage = message.get("usage") or {}
        if not usage:
            continue
        context = (
            (usage.get("input") or 0)
            + (usage.get("cacheRead") or 0)
            + (usage.get("cacheWrite") or 0)
        )
        if context <= 0:
            continue
        if event.get("type") == "message_end":
            out["turns"] += 1
        out["cache_read_total"] += usage.get("cacheRead") or 0
        if out["model"] is None:
            out["model"] = message.get("model")
        if out["turn1_context"] is None:
            out["turn1_context"] = context
        total = usage.get("totalTokens") or (context + (usage.get("output") or 0))
        if total:
            out["final_tokens"] = max(out["final_tokens"] or 0, total)
        cost = (usage.get("cost") or {}).get("total")
        if cost:
            out["cost_usd"] = max(out["cost_usd"] or 0.0, cost)
    return out


def parse_opencode_transcript(path: Path) -> dict:
    """Turn-1 context and totals from an opencode `run --format json` log.

    opencode moves the usage object around between versions, so every event
    is searched for a `tokens`-shaped dict rather than keyed off one path.
    """
    out = {"turn1_context": None, "final_tokens": None, "cost_usd": None,
           "turns": 0, "model": None, "cache_read_total": 0}

    def find_tokens(node):
        if isinstance(node, dict):
            if "tokens" in node and isinstance(node["tokens"], dict):
                yield node["tokens"]
            for value in node.values():
                yield from find_tokens(value)
        elif isinstance(node, list):
            for value in node:
                yield from find_tokens(value)

    for event in _iter_json_lines(path):
        if out["model"] is None:
            model = event.get("model") or (event.get("info") or {}).get("model")
            if isinstance(model, str):
                out["model"] = model
        for tokens in find_tokens(event):
            cache = tokens.get("cache")
            cache_read = cache_write = 0
            if isinstance(cache, dict):
                cache_read = cache.get("read") or 0
                cache_write = cache.get("write") or 0
            context = (tokens.get("input") or 0) + cache_read + cache_write
            if context <= 0:
                continue
            out["turns"] += 1
            out["cache_read_total"] += cache_read
            if out["turn1_context"] is None:
                out["turn1_context"] = context
            total = context + (tokens.get("output") or 0)
            out["final_tokens"] = max(out["final_tokens"] or 0, total)
    return out


PARSERS = {
    "claude": parse_claude_transcript,
    "codex": parse_codex_transcript,
    "omp": parse_pi_family_transcript,
    "pi": parse_pi_family_transcript,
    "opencode": parse_opencode_transcript,
}


# --- collection -----------------------------------------------------------

SESSION_RE = re.compile(r"^ses-([a-z0-9]+)-")


def collect(logs_dir: Path, limit: int | None = None) -> list[dict]:
    rows = []
    prompts = sorted(logs_dir.glob("*.prompt.txt"))
    if limit:
        prompts = prompts[-limit:]
    for prompt_path in prompts:
        session = prompt_path.name[: -len(".prompt.txt")]
        match = SESSION_RE.match(session)
        backend = match.group(1) if match else "unknown"
        try:
            text = prompt_path.read_text(errors="replace")
        except OSError:
            continue

        seg = split_prompt(text)
        row = {
            "session": session,
            "backend": backend,
            "role": detect_role(text),
            "bytes": len(text),
            "total_tok": ntok(text),
            "mtime": prompt_path.stat().st_mtime,
        }
        for name, chunk in seg.items():
            row[f"tok_{name}"] = ntok(chunk)
        # Everything the board writes that is not the human's own words.
        row["tok_overhead"] = (
            row["tok_header"] + row["tok_contract"]
            + row["tok_role_block"] + row["tok_isolation"]
        )

        transcript = logs_dir / f"{session}.transcript.jsonl"
        row["transcript"] = transcript.exists()
        if transcript.exists():
            parser = PARSERS.get(backend)
            if parser:
                row.update({f"run_{k}": v for k, v in parser(transcript).items()})
        rows.append(row)
    return rows


def annotate_tasks(rows: list[dict], logs_dir: Path) -> None:
    """Attach each prompt's task id and its index among that task's launches."""
    for row in rows:
        path = logs_dir / f"{row['session']}.prompt.txt"
        try:
            first = path.open("r", errors="replace").readline()
        except OSError:
            continue
        match = re.match(r"Task: ([A-Za-z0-9_-]+):", first)
        row["task_id"] = match.group(1) if match else None

    by_task = defaultdict(list)
    for row in rows:
        if row.get("task_id"):
            by_task[row["task_id"]].append(row)
    for group in by_task.values():
        group.sort(key=lambda r: r["mtime"])
        for index, row in enumerate(group):
            row["launch_idx"] = index


def stage_of(row: dict) -> str:
    """Where in a task's life this prompt was built.

    Prompts older than the role-block change have role 'unknown'; they are
    reported separately rather than guessed at, so the current-format
    numbers stay clean.
    """
    role = row["role"]
    if role == "unknown":
        return "0. legacy prompt format"
    if role == "designer":
        return "1. designer (plan pass)"
    if role == "reviewer":
        return "5. reviewer"
    if role == "revert":
        return "6. revert"
    if row.get("launch_idx", 0) > 0:
        return "4. executor relaunch/resume"
    return "3. executor first launch (w/ thread)" if row["tok_thread"] \
        else "2. executor first launch"


# --- reporting ------------------------------------------------------------

def pct(part: float, whole: float) -> str:
    if not whole:
        return "  n/a"
    return f"{100.0 * part / whole:5.1f}%"


def med(values):
    values = [v for v in values if v is not None]
    return statistics.median(values) if values else None


def fmt(value, width=7):
    if value is None:
        return " " * (width - 3) + "n/a"
    return f"{value:{width},.0f}"


def report(rows: list[dict]) -> None:
    if not rows:
        print("no prompts found")
        return

    print("=" * 78)
    print("KANBAN4AI TOKEN PROFILE")
    print(f"sessions: {len(rows)}   tokenizer: "
          f"{'tiktoken/cl100k_base' if _ENC else 'chars/4 fallback'}")
    print("=" * 78)

    # 1. What the board itself injects, per stage of a task's life.
    print("\n[1] KANBAN-AUTHORED PROMPT BY TASK LIFECYCLE STAGE (median tokens)")
    print("    (what the board costs before the backend adds anything)\n")
    head = (f"{'stage':<36} {'n':>4} {'total':>7} {'contract':>9} {'role':>6} "
            f"{'isol':>6} {'task':>7} {'thread':>8} {'fixed':>7}")
    print("    " + head)
    print("    " + "-" * len(head))
    by_stage = defaultdict(list)
    for row in rows:
        by_stage[stage_of(row)].append(row)
    for stage, group in sorted(by_stage.items()):
        fixed = med([r["tok_overhead"] for r in group]) or 0
        total = med([r["total_tok"] for r in group]) or 0
        print(f"    {stage:<36} {len(group):>4} {fmt(total)} "
              f"{fmt(med([r['tok_contract'] for r in group]), 9)} "
              f"{fmt(med([r['tok_role_block'] for r in group]), 6)} "
              f"{fmt(med([r['tok_isolation'] for r in group]), 6)} "
              f"{fmt(med([r['tok_user_task'] for r in group]), 7)} "
              f"{fmt(med([r['tok_thread'] for r in group]), 8)} "
              f"{pct(fixed, total):>7}")

    # 2. Where the board's own bytes go.
    print("\n[2] BOARD OVERHEAD BREAKDOWN (all sessions, median tokens)")
    print("    'board-fixed' is text the human never wrote and that repeats\n"
          "    identically on every single launch and relaunch.\n")
    for name, label in [
        ("tok_header", "header (task id/title/project path)"),
        ("tok_contract", "session contract (the big one)"),
        ("tok_role_block", "role + column ownership block"),
        ("tok_isolation", "worktree isolation notice"),
        ("tok_user_task", "-> user's actual task text"),
        ("tok_thread", "-> replayed thread context"),
    ]:
        value = med([r[name] for r in rows]) or 0
        print(f"      {label:<40} {fmt(value)}")
    fixed = med([r["tok_overhead"] for r in rows]) or 0
    total = med([r["total_tok"] for r in rows]) or 0
    print(f"      {'':<40} {'-' * 7}")
    print(f"      {'board-fixed subtotal':<40} {fmt(fixed)}  "
          f"({pct(fixed, total).strip()} of median prompt)")

    # 3. Backend tax, measured not estimated.
    print("\n[3] BACKEND TAX — real turn-1 context vs the kanban prompt")
    print("    turn-1 context = what the model actually read on call #1.")
    print("    tax = that minus the kanban prompt: the backend's own system")
    print("    prompt, tool schemas and AGENTS.md/CLAUDE.md injection.\n")
    head = (f"{'backend':<10} {'n':>4} {'kanban':>8} {'turn-1 ctx':>11} "
            f"{'backend tax':>12} {'kanban share':>13}")
    print("    " + head)
    print("    " + "-" * len(head))
    by_backend = defaultdict(list)
    for row in rows:
        by_backend[row["backend"]].append(row)
    for backend, group in sorted(by_backend.items(), key=lambda kv: -len(kv[1])):
        withctx = [r for r in group if r.get("run_turn1_context")]
        if not withctx:
            print(f"    {backend:<10} {len(group):>4} "
                  f"{fmt(med([r['total_tok'] for r in group]), 8)} "
                  f"{'n/a':>11} {'n/a':>12} {'n/a':>13}")
            continue
        kanban = med([r["total_tok"] for r in withctx]) or 0
        ctx = med([r["run_turn1_context"] for r in withctx]) or 0
        tax = med([r["run_turn1_context"] - r["total_tok"] for r in withctx]) or 0
        print(f"    {backend:<10} {len(withctx):>4} {fmt(kanban, 8)} "
              f"{fmt(ctx, 11)} {fmt(tax, 12)} {pct(kanban, ctx):>13}")

    # 4. Repetition: the same task launched over and over.
    print("\n[4] RELAUNCH AMPLIFICATION — board-fixed tokens re-paid per task")
    print("    Every relaunch rebuilds the whole prompt from scratch, so the")
    print("    contract is billed again each time and the thread replay grows.\n")
    by_task = defaultdict(list)
    for row in rows:
        if row.get("task_id"):
            by_task[row["task_id"]].append(row)
    multi = {k: sorted(v, key=lambda r: r["mtime"])
             for k, v in by_task.items() if len(v) > 1}
    if multi:
        launches = [len(v) for v in multi.values()]
        wasted = sum(sum(r["tok_overhead"] for r in v[1:]) for v in multi.values())
        total_fixed = sum(sum(r["tok_overhead"] for r in v) for v in by_task.values())
        print(f"      tasks launched more than once      {len(multi):>7} "
              f"of {len(by_task)}")
        print(f"      median launches for those tasks    {fmt(med(launches))}")
        print(f"      max launches for one task          {fmt(max(launches))}")
        print(f"      board-fixed tokens, all launches   {fmt(total_fixed, 7)}")
        print(f"      ...of which re-paid on relaunch    {fmt(wasted, 7)}  "
              f"({pct(wasted, total_fixed).strip()})")
        print("\n      worst offenders (task: launches, board-fixed tokens burned)")
        worst = sorted(multi.items(),
                       key=lambda kv: -sum(r["tok_overhead"] for r in kv[1]))[:8]
        for task, group in worst:
            burned = sum(r["tok_overhead"] for r in group)
            thread = group[-1]["tok_thread"]
            print(f"        {task:<12} {len(group):>3} launches  "
                  f"{burned:>7,} fixed tok   final thread replay {thread:>6,}")

    # 5. Thread growth: the part that scales with task age.
    print("\n[5] THREAD REPLAY GROWTH — the cost that compounds")
    threads = sorted((r["tok_thread"] for r in rows if r["tok_thread"]), reverse=True)
    if threads:
        print(f"      prompts carrying a thread replay   {len(threads):>7} "
              f"of {len(rows)}")
        print(f"      median thread replay               {fmt(med(threads))}")
        print(f"      p90 thread replay                  "
              f"{fmt(threads[max(0, int(len(threads) * 0.10))])}")
        print(f"      largest thread replay              {fmt(threads[0])}")
        heavy = [r for r in rows if r["tok_thread"] > r["tok_overhead"]]
        print(f"      prompts where thread > contract    {len(heavy):>7} "
              f"({pct(len(heavy), len(rows)).strip()})")

    # 6. The absolute totals.
    print("\n[6] LIFETIME TOTALS ACROSS THIS BOARD'S HISTORY")
    all_fixed = sum(r["tok_overhead"] for r in rows)
    all_thread = sum(r["tok_thread"] for r in rows)
    all_task = sum(r["tok_user_task"] for r in rows)
    all_prompt = sum(r["total_tok"] for r in rows)
    print(f"      kanban prompt tokens, all sessions {fmt(all_prompt, 9)}")
    print(f"        board-fixed boilerplate          {fmt(all_fixed, 9)}  "
          f"{pct(all_fixed, all_prompt).strip()}")
    print(f"        replayed thread context          {fmt(all_thread, 9)}  "
          f"{pct(all_thread, all_prompt).strip()}")
    print(f"        the human's actual task text     {fmt(all_task, 9)}  "
          f"{pct(all_task, all_prompt).strip()}")
    ctxrows = [r for r in rows if r.get("run_turn1_context")]
    if ctxrows:
        real = sum(r["run_turn1_context"] for r in ctxrows)
        kb = sum(r["total_tok"] for r in ctxrows)
        print(f"      measured turn-1 context, {len(ctxrows):>3} runs   {fmt(real, 9)}")
        print(f"        attributable to kanban           {fmt(kb, 9)}  "
              f"{pct(kb, real).strip()}")
        print(f"        backend system prompt + tools    {fmt(real - kb, 9)}  "
              f"{pct(real - kb, real).strip()}")
    print()


def cross_project(store: Path) -> int:
    """Fit `backend tax = floor + slope * AGENTS.md` across every board.

    One board cannot separate a backend's fixed system-prompt/tool cost from
    the repo docs it auto-loads: both are constant within that board. Several
    boards with different-sized AGENTS.md files can. The intercept is the
    backend's own floor; the slope is how much of AGENTS.md it actually pulls
    into context (claude comes out at ~0 because it reads CLAUDE.md instead).
    """
    points = defaultdict(list)
    rowsper = []
    for proj in sorted(p for p in store.iterdir() if p.is_dir()):
        meta = proj / "project.yaml"
        logs = proj / ".kanban" / "logs"
        if not meta.is_file() or not logs.is_dir():
            continue
        match = re.search(r"path:\s*(.+)", meta.read_text(errors="replace"))
        if not match:
            continue
        src = Path(match.group(1).strip().strip("\"'"))
        agents = src / "AGENTS.md"
        agents_tok = ntok(agents.read_text(errors="replace")) if agents.is_file() else 0

        rows = collect(logs)
        by_backend = defaultdict(list)
        for row in rows:
            if row.get("run_turn1_context"):
                by_backend[row["backend"]].append(
                    row["run_turn1_context"] - row["total_tok"])
        for backend, taxes in by_backend.items():
            if len(taxes) < 3:
                continue
            tax = statistics.median(taxes)
            points[backend].append((agents_tok, tax))
            rowsper.append((proj.name, agents_tok, backend, len(taxes), tax))

    print("=" * 78)
    print("CROSS-PROJECT BACKEND COST MODEL")
    print("=" * 78)
    print(f"\n{'project':<16}{'AGENTS.md':>10}  {'backend':<9}{'runs':>5}{'median tax':>12}")
    print("-" * 54)
    for name, agents_tok, backend, n, tax in rowsper:
        print(f"{name:<16}{agents_tok:>10,}  {backend:<9}{n:>5}{tax:>12,.0f}")

    print("\nFIT: turn-1 backend tax = floor + slope x AGENTS.md tokens\n")
    head = (f"{'backend':<10}{'boards':>7}{'floor (sys+tools)':>19}"
            f"{'AGENTS slope':>14}{'r':>7}  interpretation")
    print(head)
    print("-" * (len(head) + 18))
    for backend, pts in sorted(points.items(), key=lambda kv: -len(kv[1])):
        n = len(pts)
        if n < 3:
            continue
        mx = sum(x for x, _ in pts) / n
        my = sum(y for _, y in pts) / n
        den = sum((x - mx) ** 2 for x, _ in pts)
        if not den:
            continue
        num = sum((x - mx) * (y - my) for x, y in pts)
        slope = num / den
        intercept = my - slope * mx
        vy = sum((y - my) ** 2 for _, y in pts)
        r = num / ((den * vy) ** 0.5) if vy else 0.0
        note = ("loads AGENTS.md in full" if slope > 0.8
                else "loads AGENTS.md partially" if slope > 0.25
                else "ignores AGENTS.md")
        print(f"{backend:<10}{n:>7}{intercept:>19,.0f}{slope:>14.2f}{r:>7.2f}  {note}")

    print("\nWHAT THIS BOARD'S OWN AGENTS.md COSTS PER SESSION")
    here = store / "kanban4ai" / "project.yaml"
    agents_tok = 0
    if here.is_file():
        match = re.search(r"path:\s*(.+)", here.read_text(errors="replace"))
        if match:
            path = Path(match.group(1).strip().strip("\"'")) / "AGENTS.md"
            if path.is_file():
                agents_tok = ntok(path.read_text(errors="replace"))
    if agents_tok:
        print(f"  AGENTS.md = {agents_tok:,} tokens\n")
        for backend, pts in sorted(points.items()):
            n = len(pts)
            mx = sum(x for x, _ in pts) / n
            my = sum(y for _, y in pts) / n
            den = sum((x - mx) ** 2 for x, _ in pts)
            if not den:
                continue
            slope = sum((x - mx) * (y - my) for x, y in pts) / den
            print(f"    {backend:<10} {max(0.0, slope) * agents_tok:>9,.0f} "
                  f"tokens of context window, every single session")
    print()
    return 0


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__,
                                     formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("--logs", type=Path, default=None,
                        help="board logs dir (default: $KANBAN_DATA/logs or ./.kanban/logs)")
    parser.add_argument("--json", action="store_true", help="emit raw rows as JSON")
    parser.add_argument("--limit", type=int, default=None,
                        help="only profile the N most recent sessions")
    parser.add_argument("--cross-project", type=Path, nargs="?",
                        const=Path.home() / ".local/share/kanban4ai/projects",
                        default=None,
                        help="fit the backend cost model across every board in the store")
    args = parser.parse_args()

    if args.cross_project is not None:
        if not args.cross_project.is_dir():
            print(f"no project store at {args.cross_project}", file=sys.stderr)
            return 1
        return cross_project(args.cross_project)

    logs = args.logs
    if logs is None:
        env = os.environ.get("KANBAN_DATA")
        candidates = [Path(env) / "logs"] if env else []
        candidates.append(Path.cwd() / ".kanban" / "logs")
        logs = next((c for c in candidates if c.is_dir()), candidates[-1])
    if not logs.is_dir():
        print(f"no logs directory at {logs}", file=sys.stderr)
        return 1

    rows = collect(logs, args.limit)
    annotate_tasks(rows, logs)
    if args.json:
        json.dump(rows, sys.stdout, indent=2, default=str)
        print()
        return 0
    report(rows)
    return 0


if __name__ == "__main__":
    sys.exit(main())
