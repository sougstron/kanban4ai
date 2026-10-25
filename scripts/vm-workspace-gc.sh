#!/bin/sh
# Disk GC for kanban4ai checkouts on the shared test VM (VM 103, TASK-564).
#
# Usage: sh scripts/vm-workspace-gc.sh [-n]
#
# Agents copy the repo to $ROOT/<task-run>/ and build there; a debug test build
# leaves a 2-8 GB target/ that nobody removes. This script, run hourly by a
# systemd timer on the VM, deletes:
#   - <run>/target       idle for TARGET_IDLE_MIN minutes (default 360);
#   - <run>              idle for RUN_IDLE_DAYS days (default 7);
#   - /tmp/kanban-*      idle for TMP_IDLE_DAYS days (default 2).
# "Idle" means no file under it changed in that time and no process has its cwd
# inside it, so a running build, test or E2E is never touched. -n only prints.
set -eu

ROOT=${ROOT:-/home/tester/test-workspaces/kanban4ai}
TARGET_IDLE_MIN=${TARGET_IDLE_MIN:-360}
RUN_IDLE_DAYS=${RUN_IDLE_DAYS:-7}
TMP_IDLE_DAYS=${TMP_IDLE_DAYS:-2}
DRY=0
[ "${1:-}" = -n ] && DRY=1

# Working directories of all live processes, one per line.
busy=$(for p in /proc/[0-9]*; do readlink "$p/cwd" 2>/dev/null || :; done)

in_use() {
	printf '%s\n' "$busy" | grep -q -F -x -e "$1" && return 0
	printf '%s\n' "$busy" | grep -q -F -e "$1/"
}

# fresh DIR FIND-AGE-TEST: true when something under DIR is newer than the age.
fresh() {
	[ -n "$(find "$1" "$2" "$3" -print -quit 2>/dev/null)" ]
}

drop() {
	size=$(du -sh "$1" 2>/dev/null | cut -f1)
	if [ "$DRY" = 1 ]; then
		echo "would remove $1 ($size)"
	else
		rm -rf -- "$1" && echo "removed $1 ($size)"
	fi
}

if [ -d "$ROOT" ]; then
	for run in "$ROOT"/*/; do
		run=${run%/}
		[ -d "$run" ] || continue
		in_use "$run" && continue
		if ! fresh "$run" -mtime "-$RUN_IDLE_DAYS"; then
			drop "$run"
		elif [ -d "$run/target" ] && ! fresh "$run/target" -mmin "-$TARGET_IDLE_MIN"; then
			drop "$run/target"
		fi
	done
fi

for t in /tmp/kanban-*; do
	[ -d "$t" ] && [ -O "$t" ] || continue
	in_use "$t" && continue
	fresh "$t" -mtime "-$TMP_IDLE_DAYS" || drop "$t"
done
