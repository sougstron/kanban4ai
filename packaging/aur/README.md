# AUR packaging

- `kanban4ai/` builds the tagged stable source archive with Cargo's lockfile.
- `kanban4ai-git/` builds the current `main` branch and derives `pkgver` from
  Git tags.

The stable recipe pins the SHA-256 checksum of the matching GitHub tag archive.
When updating it, set `pkgver`, download the exact source archive, refresh the
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
