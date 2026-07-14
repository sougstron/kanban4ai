# AUR packaging

- `kanban4ai/` builds the tagged stable source archive with Cargo's lockfile.
- `kanban4ai-git/` builds the current `main` branch and derives `pkgver` from
  Git tags.

The stable recipe intentionally uses `SKIP` only while the first `v0.1.0`
source archive does not exist. Before publishing or updating the AUR package,
download the exact GitHub source archive, replace `SKIP` with its SHA-256
checksum, increment `pkgrel` when appropriate, and regenerate `.SRCINFO`:

```sh
cd packaging/aur/kanban4ai
updpkgsums
makepkg --printsrcinfo > .SRCINFO
```

For the VCS package, regenerate `.SRCINFO` after material package changes:

```sh
cd packaging/aur/kanban4ai-git
makepkg --printsrcinfo > .SRCINFO
```
