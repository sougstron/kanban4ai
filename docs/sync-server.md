# Sync server: how to run and connect nodes

TASK-514 cutover: machines exchange store operations **only through one
sync-server**. Clients (`laptop`, `mobile`) never sync with each other; the
server is the channel, not a condition of work. Without a server two machines
cannot sync directly — that price is accepted. Local-first still holds: every
node keeps a full replica, and a server that is down does not block local
writes (operations wait in the outbox).

Spec: [sync.md](sync.md) sections 0–16. This file is the operational how-to.

## 1. Start the server

The server is an ordinary node with `role: server` in the global config, plus
a sync "hub" that assigns each accepted operation a global sequence number
(seq) and keeps a journal of all operations.

```yaml
# <store>/config.yaml on the server
node:
  role: server
  name: server
serve:
  port: 7420
```

Run `kanban4ai serve` in the foreground, or install the systemd unit:

- AUR / `sh scripts/install.sh --with-serve` puts
  `kanban4ai-serve.service` in place; enable it with
  `systemctl --user enable --now kanban4ai-serve.service`.
- Logs: `journalctl --user -u kanban4ai-serve.service`.

The server listens on the LAN addresses chosen by `sync.home_networks` plus
loopback, and on the tailnet IP when `net.tailscale.mode: system`. Loopback is
always available, which is what the reverse proxy uses.

### Pairing codes

```sh
kanban4ai serve --pair-code          # one-time 6-digit code, 5 min, 5 attempts
```

The server also answers `POST /pair` only — clients do not pair with each
other, and the server no longer issues a `reverse_token` (it never dials
clients back).

## 2. Connect a machine

On the client (`laptop` or `mobile`):

```sh
kanban4ai serve --pair <server-host:port> --code <CODE>
```

`--pair` accepts an `http://` or `https://` URL (https for reverse proxies).
`--address` was removed with the peer model; the client's address is not sent
to the server.

The server registers the client in `sync/hub/clients.json` (node id, name,
role, token hash) and the client stores its own half in
`sync/server.yaml` (0600: `{node_id, token, urls}`). The global config gains
`sync.server: {node_id, name, urls: [...]}` (machine-local). The existing
node tokens keep working; revoking a token (`serve --revoke-token <sha256>`)
removes the client from `clients.json`, and its cursor stops holding back hub
compaction.

### Phone

`kanban4ai serve --phone-qr [HOST:PORT]` on the server prints the invite and a
QR with a one-time code; on the phone run

```sh
kanban4ai join CODE@HOST:PORT
```

`join` makes the node `mobile`/outbound, turns on managed Tailscale for
tailnet hosts, pairs with the server, and runs one sync cycle. `join --fresh`
first moves a store that already holds another board to
`<store>.pre-join-<time>` (nothing is deleted) and joins with an empty store;
without it, a different non-empty store fails with `SYNC_STORE_MISMATCH`.
`join` against a node that is not `role: server` fails with
`SYNC_NOT_A_SERVER`.

A brand-new client pulls the board as a **streaming NDJSON snapshot**
(`GET /sync/v3/snapshot`, see sync.md §10.4), then continues with ordinary
`pull after seq`. Memory is O(one line); images are content-addressed blobs
the client fetches separately.

## 3. Public access: reverse proxy with TLS

There is no built-in TLS. The client accepts `http://` and `https://` URLs
(reqwest + rustls). For public access put the server behind a reverse proxy
pointing at `127.0.0.1:7420` (loopback is always listened on); LAN and tailnet
access stays as direct connections. The store is never written to any
project's git remote.

SSE (`/sync/v3/stream`) needs proxies that flush:

Caddy:

```
sync.example.com {
    reverse_proxy 127.0.0.1:7420 {
        flush_interval -1
    }
}
```

nginx:

```
location / {
    proxy_pass http://127.0.0.1:7420;
    proxy_buffering off;
    proxy_http_version 1.1;
    proxy_set_header Connection "";
}
```

If the proxy cuts SSE, the client automatically falls back to
`GET /sync/v3/ops?wait=25` after two failed streams.

## 4. Migration from 0.8 (peers / git-relay)

Upgrading a format-3 store runs the v4 migration on open (idempotent, `FORMAT`
written last):

1. `sync.peers` with role `server` (at most one) becomes
   `sync.server {node_id, name, urls: [http://<address>, …]}`; the token moves
   from `sync/peers.yaml` to `sync/server.yaml`. On the server itself, the old
   peers become `hub/clients.json`; **the old tokens stay valid — no
   re-pairing needed**. A setup with no server among the peers (only
   laptop↔phone) creates no `sync.server` and prints a one-time hint: connect
   a server, see this file.
2. Previous `sync.peers` / `peers.yaml` are preserved under
   `sync/legacy-v3/` (machine-local, removable by hand); the `sync.relay_*`
   keys, `exchanges.yaml`, `relay-ack.json`, `relay-age.key` and the
   `sync/relay/` directory are removed.
3. Server: the hub is initialised (epoch + ingest of the retained journal).
4. Clients with no `cursor.json` push **all** retained operations of all
   origins (except the server's own) once, so operations accumulated before
   the cutover — including ones received directly from the phone — reach the
   server; the server deduplicates. An origin whose answer head is below what
   was sent (prefix already pruned) logs `SYNC_MIGRATION_GAP` as a warning,
   not a stop. Then `pushed = heads[self]`, `pull after 0`, migration flag
   cleared.

Binary 0.8.x on a format-4 store fails with `SYNC_STORE_TOO_NEW`; update all
nodes together (a 0.8 node and a new server do not sync: old endpoints answer
`426 SYNC_PROTOCOL_MISMATCH` with "update kanban4ai on all nodes").

Old relay refs in public remotes are **not** removed automatically:

```sh
kanban4ai serve --purge-relay-refs [--dry-run]
```

deletes `refs/kanban/sync/<own node_id>` and `refs/kanban/logs/<own node_id>/*`
from the origin of every project (`git push --delete`). History that was
already cloned stays with anyone who received it.

## 5. The server is not a backup

A replica is not a backup: deletions replicate, and the hub compacts
operations by client cursors. Keep `kanban backup` snapshots
([backup.md](backup.md)) for restore; `kanban backup` includes `sync/hub/`
(without `hub/spool/`) so a restored server keeps its journal.

## 6. Behaviour without the server

All commands work: create/move/edit/thread run locally, operations accumulate
in the outbox (`sync/journal/<own node>/`), and delivery happens when the
server is reachable again. An explicit remote runner that cannot be reached
queues the request (`runner_request`, exit 0, warning). `runner: auto` =
server when the last hello/cycle succeeded, otherwise `here`. Status shows the
server path (LAN, tailnet, none) without blocking the board.
