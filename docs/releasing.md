# Updater internals

Reference detail for the updater implementation. Read it when touching
`core/update.rs` or `core/http.rs`. Maintainer releases use the project-local
[`update-app`](../.agents/skills/update-app/SKILL.md) skill instead.

## Updater (`core/update.rs`, `core/http.rs`, `kanban update`)

kanban4ai checks GitHub Releases for a newer version and can self-update an
unmanaged install. Everything is best effort: a missing curl, a network
failure, or a malformed remote tag degrades to "no answer" / "no update",
never to an error the board cannot draw.

**Check.** One unauthenticated `GET /releases/latest` through the same
curl-config helper the provider-limits fetches use (`core/http.rs`), so no
TLS stack is linked into the crate. The result is an `UpdateStatus`
persisted atomically to `<store>/update-status.json` (next to `limits.json`,
the same settings-vs-state split): `checked_at` (Unix seconds),
`latest_version`/`tag` (compared with a strict three-part numeric parse; a
tag that does not parse is never "newer"), this platform's
`asset_url`/`checksum_url` (`None` when the release workflow builds no
archive for it — fail closed, never guess a near-miss triple), `notes_url`,
`published_at`, and `dismissed_version`. UI reads only ever hit the cache
(memory, then that file); the network is paid by exactly three callers: the
TUI's on-open warm check (`core::update::warm_check`, gated by
`updates.check_on_open` and skipped inside the `updates.check_interval_hours`
TTL), a Global Settings `Check now` (one deliberate blocking check), and
`kanban update` (cache first, otherwise one blocking check). A newer release
shows a one-time status-line banner (`↑ kanban4ai X.Y.Z available - open
Settings to update`); showing persists `dismissed_version`, so the same
version never nags again but a newer tag reopens the banner. `updates.notify`
is reserved for a desktop notification and does nothing yet.

**Apply.** `kanban update` without `--check` reports when up to date and
otherwise downloads, verifies, and atomically replaces the binary — but only
for an **unmanaged** install. The pacman ownership probe (`pacman -Qo` on the
resolved `current_exe()`, locale pinned to `C`) is a hard gate, not a hint: a
package-managed binary is never self-replaced, even when the directory
happens to be writable, because overwriting it would desync pacman's file
database. Our own AUR packages (`kanban4ai`, `kanban4ai-bin`, `kanban4ai-git`)
answer with their AUR-helper upgrade command (`yay`/`paru -S <package>`, or
`sudo pacman -Syu <package>` for anything else pacman owns) and exit 0 —
pointing at the package manager did what was asked. The probe runs before
anything is fetched.

For an unmanaged install the pipeline is: probe the install directory with a
temp file (an unwritable directory answers with fix-the-permissions
guidance; self-update never reaches for sudo); confirm `curl`, `sha256sum`,
and `tar` are on PATH (the error names the missing tool; the checksum is
never skipped); download the archive and its `.sha256` sibling to temp files
next to the binary, so the final rename stays on one filesystem; parse the
published digest (`<64 hex> <name>`, case-insensitive) and compare it with
`sha256sum` of the download — a mismatch discards the download; extract the
single payload member with `tar -xO` into memory (the member path is derived
from the untrusted tag and rejected if it could leave the top-level
directory, so no archive path ever touches the filesystem); stage it as a
temp file with mode `0755` and `rename(2)` it over the running binary. Linux
swaps the directory entry while the old process keeps executing its old
inode, so nothing breaks — the output says to restart (or open a new
terminal) because the old code runs until the process exits. Every failure
path drops its temp files and leaves the existing binary untouched.

Packaging: `sha256sum` (coreutils) and `tar` ship in Arch's `base`
meta-package, so every Arch system has them and they are not listed;
`curl` stays an optdepends entry because every use degrades gracefully
without it (limits `n/a`, check "no answer") and the apply path names the
missing tool when it matters.
