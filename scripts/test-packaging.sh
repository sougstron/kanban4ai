#!/bin/sh
set -eu

project_root=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
binary=${KANBAN4AI_BINARY:-${project_root}/target/release/kanban4ai}
tmpdir=$(mktemp -d)
trap 'rm -rf "$tmpdir"' EXIT HUP INT TERM

# Pin HOME and XDG_CONFIG_HOME so the unit-file checks below never depend on
# the caller's environment (and a stray write lands in the sandbox, not $HOME).
fake_home=$tmpdir/home
fake_config=$fake_home/.config
mkdir -p "$fake_config"

DESTDIR=$tmpdir PREFIX=/usr HOME=$fake_home XDG_CONFIG_HOME=$fake_config \
    KANBAN4AI_BINARY=$binary sh "$project_root/scripts/install.sh"

bindir=$tmpdir/usr/bin
test -x "$bindir/kanban4ai"
test -L "$bindir/kanban"
test -L "$bindir/kb"
test "$(readlink "$bindir/kanban")" = kanban4ai
test "$(readlink "$bindir/kb")" = kanban4ai
test -x "$bindir/kanban"
test -x "$bindir/kb"

if DESTDIR=$tmpdir PREFIX=/usr HOME=$fake_home XDG_CONFIG_HOME=$fake_config \
    KANBAN4AI_BINARY=$binary sh "$project_root/scripts/install.sh" >/dev/null 2>&1; then
    printf '%s\n' "error: installer overwrote an existing install" >&2
    exit 1
fi

# Default install must not drop a user unit.
if [ -e "$tmpdir/usr/lib/systemd/user/kanban4ai.service" ] \
    || [ -e "$tmpdir$fake_config/systemd/user/kanban4ai.service" ]; then
    printf '%s\n' "error: installer wrote a user unit without --with-daemon" >&2
    exit 1
fi

# Opt-in daemon install: unit lands under DESTDIR+HOME, is never enabled.
daemon_tmp=$(mktemp -d)
trap 'rm -rf "$tmpdir" "$daemon_tmp"' EXIT HUP INT TERM
home=$daemon_tmp/home
config=$home/.config
mkdir -p "$config"
DESTDIR=$daemon_tmp PREFIX=/usr HOME=$home XDG_CONFIG_HOME=$config \
    KANBAN4AI_BINARY=$binary \
    sh "$project_root/scripts/install.sh" --with-daemon >/dev/null
unit=$daemon_tmp$config/systemd/user/kanban4ai.service
test -f "$unit"
grep -q '^ExecStart=/usr/bin/kanban4ai daemon$' "$unit"
grep -q '^Type=simple$' "$unit"
grep -q '^Restart=on-failure$' "$unit"
grep -q '^RestartSec=30$' "$unit"
if [ -e "$config/systemd/user/default.target.wants/kanban4ai.service" ] \
    || [ -e "$daemon_tmp$config/systemd/user/default.target.wants/kanban4ai.service" ]; then
    printf '%s\n' "error: installer enabled the user unit" >&2
    exit 1
fi

printf '%s\n' "packaging smoke test passed"
