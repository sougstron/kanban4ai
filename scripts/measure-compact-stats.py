#!/usr/bin/env python3
"""Isolated, reproducible stats output comparison; retains all artifacts."""
import argparse
import json
import os
from pathlib import Path
import subprocess
import tempfile

parser = argparse.ArgumentParser(description=__doc__)
parser.add_argument("binary", type=Path)
args = parser.parse_args()
binary = args.binary.resolve()
root = Path(tempfile.mkdtemp(prefix="kanban-compact-"))
work = root / "work"
work.mkdir()
env = {k: v for k, v in os.environ.items() if not k.startswith("KANBAN_")}
env["KANBAN_HOME"] = str(root / "store")


def run(*arguments):
    result = subprocess.run(
        [str(binary), *arguments], cwd=work, env=env,
        text=True, capture_output=True, check=False,
    )
    if result.returncode:
        raise RuntimeError(f"{arguments}: {result.returncode}\n{result.stderr}")
    if result.stderr:
        print(result.stderr, end="")
    return result.stdout


run("init")
boards = list((root / "store").glob("projects/*/.kanban"))
assert len(boards) == 1, boards
stats = boards[0] / "stats"
stats.mkdir(exist_ok=True)
records = [
    dict(kind="usage", ts="2026-01-01T12:00:00", task_id=f"TASK-{i}",
         tokens=1000, backend=f"backend-{i:03}", model=f"provider-{i:03}/model")
    for i in range(80)
]
(stats / "events.jsonl").write_text("".join(json.dumps(r) + "\n" for r in records))
full = run("stats")
compact = run("stats", "--compact")
(root / "before.txt").write_text(full)
(root / "after.txt").write_text(compact)
report = Path(next(line.removeprefix("Full report: ") for line in compact.splitlines()
                   if line.startswith("Full report: ")))
assert report.read_text() + "\n" == full
for line in full.splitlines():
    if line.startswith(("Total", "Average")):
        assert line in compact.splitlines(), line
assert "80,000" in compact
print(f"before={len(full)} chars; after={len(compact)} chars; "
      f"reduction={100 * (1 - len(compact) / len(full)):.1f}%")
print(f"Artifacts: {root}")
print(f"Full report: {report}")
