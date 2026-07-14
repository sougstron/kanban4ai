#!/bin/sh
set -eu

project_root=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
binary=${KANBAN4AI_BINARY:-${project_root}/target/release/kanban4ai}
tmpdir=$(mktemp -d)
trap 'rm -rf "$tmpdir"' EXIT HUP INT TERM

DESTDIR=$tmpdir PREFIX=/usr KANBAN4AI_BINARY=$binary \
    sh "$project_root/scripts/install.sh"

bindir=$tmpdir/usr/bin
test -x "$bindir/kanban4ai"
test -L "$bindir/kanban"
test -L "$bindir/kb"
test "$(readlink "$bindir/kanban")" = kanban4ai
test "$(readlink "$bindir/kb")" = kanban4ai
test -x "$bindir/kanban"
test -x "$bindir/kb"

if DESTDIR=$tmpdir PREFIX=/usr KANBAN4AI_BINARY=$binary \
    sh "$project_root/scripts/install.sh" >/dev/null 2>&1; then
    printf '%s\n' "error: installer overwrote an existing install" >&2
    exit 1
fi

printf '%s\n' "packaging smoke test passed"
