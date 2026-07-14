#!/bin/sh
set -eu

prefix=${PREFIX:-/usr/local}
destdir=${DESTDIR:-}
source_binary=${KANBAN4AI_BINARY:-target/release/kanban4ai}
bindir=${destdir}${prefix}/bin
canonical=${bindir}/kanban4ai

if [ ! -f "$source_binary" ] || [ ! -x "$source_binary" ]; then
    printf '%s\n' "error: executable release binary not found: $source_binary" >&2
    printf '%s\n' "build it with: cargo build --release --locked" >&2
    exit 1
fi

if [ -e "$canonical" ] || [ -L "$canonical" ]; then
    printf '%s\n' "error: refusing to overwrite existing path: $canonical" >&2
    exit 1
fi

for alias in kanban kb; do
    alias_path=${bindir}/${alias}
    if [ -e "$alias_path" ] || [ -L "$alias_path" ]; then
        printf '%s\n' "error: refusing to overwrite existing path: $alias_path" >&2
        exit 1
    fi
done

mkdir -p "$bindir"
install -m 0755 "$source_binary" "$canonical"

if ! ln -s kanban4ai "${bindir}/kanban"; then
    rm -f "$canonical"
    exit 1
fi
if ! ln -s kanban4ai "${bindir}/kb"; then
    rm -f "${bindir}/kanban" "$canonical"
    exit 1
fi

printf '%s\n' "installed $canonical"
printf '%s\n' "created aliases ${bindir}/kanban and ${bindir}/kb"
