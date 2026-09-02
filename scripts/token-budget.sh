#!/bin/sh
# Guard the token cost of the docs that agent backends auto-load.
#
# opencode, omp and pi pull AGENTS.md into the context window of *every*
# session they start; claude does the same with CLAUDE.md. Those files are
# therefore not free documentation — each token in them is a token removed
# from the working context of every agent run, on every relaunch, for the
# whole life of the board.
#
# This script fails when they grow past their budget, so the saving does not
# silently erode. Wire it into the same place as `cargo fmt --check`.
#
# Budgets are token counts and can be overridden from the environment:
#   AGENTS_BUDGET (default 6000)   CLAUDE_BUDGET (default 1500)
#
# Counting uses tiktoken when a python with it is on PATH, otherwise a
# bytes/4 estimate, which is close enough for a budget gate.

set -eu

root=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
: "${AGENTS_BUDGET:=6000}"
: "${CLAUDE_BUDGET:=1500}"

count_tokens() {
    file=$1
    [ -f "$file" ] || { echo 0; return; }
    if [ -n "${TOKEN_PYTHON:-}" ] && "$TOKEN_PYTHON" -c 'import tiktoken' 2>/dev/null; then
        "$TOKEN_PYTHON" - "$file" <<'PY'
import sys, tiktoken
enc = tiktoken.get_encoding("cl100k_base")
with open(sys.argv[1], errors="replace") as handle:
    print(len(enc.encode(handle.read(), disallowed_special=())))
PY
    elif python3 -c 'import tiktoken' 2>/dev/null; then
        python3 - "$file" <<'PY'
import sys, tiktoken
enc = tiktoken.get_encoding("cl100k_base")
with open(sys.argv[1], errors="replace") as handle:
    print(len(enc.encode(handle.read(), disallowed_special=())))
PY
    else
        # bytes/4 fallback; deliberately rounds up so the gate stays strict.
        bytes=$(wc -c <"$file")
        echo $(( (bytes + 3) / 4 ))
    fi
}

status=0

check() {
    file=$1
    budget=$2
    label=$3
    path="$root/$file"
    if [ ! -f "$path" ]; then
        printf '  %-12s %8s  (absent)\n' "$file" "-"
        return
    fi
    tokens=$(count_tokens "$path")
    if [ "$tokens" -gt "$budget" ]; then
        printf '  %-12s %8s tokens  OVER budget %s  <- %s\n' \
            "$file" "$tokens" "$budget" "$label"
        status=1
    else
        printf '  %-12s %8s tokens  ok (budget %s)\n' "$file" "$tokens" "$budget"
    fi
}

echo "Auto-loaded agent docs — context cost per session:"
check AGENTS.md "$AGENTS_BUDGET" "read by opencode/omp/pi on every session"
check CLAUDE.md "$CLAUDE_BUDGET" "read by claude on every session"

if [ "$status" -ne 0 ]; then
    cat <<'EOF'

An over-budget file is charged to every agent session this board starts.
Move the long-form material (keyboard tables, per-module implementation
notes, change logs) into docs/ and leave a one-line pointer, so agents load
it only when a task actually needs it.

Run scripts/profile-tokens.py --cross-project to see the measured
per-backend cost of the current size.
EOF
fi

exit "$status"
