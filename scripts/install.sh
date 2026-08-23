#!/bin/sh
set -eu

prefix=${PREFIX:-/usr/local}
destdir=${DESTDIR:-}
source_binary=${KANBAN4AI_BINARY:-target/release/kanban4ai}
bindir=${destdir}${prefix}/bin
canonical=${bindir}/kanban4ai
with_daemon=0

for arg in "$@"; do
    case $arg in
        --with-daemon) with_daemon=1 ;;
        *)
            printf '%s\n' "error: unknown option: $arg" >&2
            exit 1
            ;;
    esac
done

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

if [ "$with_daemon" -eq 1 ]; then
    script_dir=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
    unit_src=$script_dir/../packaging/systemd/kanban4ai.service
    if [ ! -f "$unit_src" ]; then
        printf '%s\n' "error: systemd unit not found: $unit_src" >&2
        exit 1
    fi
    unit_dir=${XDG_CONFIG_HOME:-${HOME}/.config}/systemd/user
    if [ -n "$destdir" ]; then
        unit_dir=${destdir}${unit_dir}
    fi
    mkdir -p "$unit_dir"
    unit_dest=$unit_dir/kanban4ai.service
    # Rewrite ExecStart to the prefix this install used; never enable the unit.
    sed "s|^ExecStart=.*|ExecStart=${prefix}/bin/kanban4ai daemon|" \
        "$unit_src" >"$unit_dest"
    printf '%s\n' "installed user unit $unit_dest (not enabled)"
    printf '%s\n' "enable with: systemctl --user enable --now kanban4ai.service"
    printf '%s\n' "cron fallback: * * * * * kanban daemon --once"
fi
