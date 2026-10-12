#!/bin/sh
# TASK-431: run inside Arch proot; see android-spike.md for the phone checklist.
set -eu
umask 077
state_dir=${HOME:?}/.local/share/kanban4ai/net/tailscale
socket=$state_dir/tailscaled.sock
peer=${SPIKE_PEER:-kanban-server}
port=${SPIKE_PORT:-7420}

usage() {
    printf '%s\n' 'Usage: sh docs/android-spike.sh install|daemon|up|check|diagnose' \
        'SPIKE_PEER=kanban-server (or full MagicDNS name), SPIKE_PORT=7420' \
        'daemon stays in the foreground; run other commands in a second proot shell.'
}

case ${1:-help} in
    install)
        # Run as the proot root user. No systemd or Android VpnService needed.
        exec pacman -S --needed tailscale curl
        ;;
    daemon)
        mkdir -p "$state_dir"
        chmod 700 "$state_dir"
        exec tailscaled --tun=userspace-networking \
            --socks5-server=127.0.0.1:1055 --statedir="$state_dir" --socket="$socket"
        ;;
    up)
        exec tailscale --socket="$socket" up --hostname kanban-phone
        ;;
    check)
        tailscale --socket="$socket" status
        # Resolve MagicDNS at the SOCKS server; ignore inherited NO_PROXY bypasses.
        # Failure (including HTTP >=400) is a nonzero exit, not a positive verdict.
        curl --fail --show-error --silent --connect-timeout 10 --max-time 30 \
            --noproxy '' --socks5-hostname 127.0.0.1:1055 \
            "http://$peer:$port/api/v1/health"
        printf '\n'
        ;;
    diagnose)
        # Diagnostic failures must not hide the remaining evidence.
        result=0
        tailscale version || result=1
        tailscale --socket="$socket" netcheck || result=1
        tailscale --socket="$socket" ping --c=5 --timeout=5s "$peer" || result=1
        tailscale --socket="$socket" status || result=1
        exit "$result"
        ;;
    help|--help|-h) usage ;;
    *) usage >&2; exit 2 ;;
esac
