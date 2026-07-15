# AUR packaging

- `kanban4ai/` builds the tagged stable source archive with Cargo's lockfile.
- `kanban4ai-bin/` installs the prebuilt GitHub release archives.
- `kanban4ai-git/` builds the current `master` branch and derives `pkgver` from
  Git tags.

The stable recipe pins the SHA-256 checksum of the matching GitHub tag archive.
When updating it, set `pkgver`, download the exact source archive, refresh the
checksum, increment `pkgrel` when appropriate, and regenerate `.SRCINFO`:

```sh
cd packaging/aur/kanban4ai
updpkgsums
makepkg --printsrcinfo > .SRCINFO
```

After the GitHub release assets exist, update the binary package's `pkgver`,
x86_64/aarch64 checksums, and `.SRCINFO`:

```sh
cd packaging/aur/kanban4ai-bin
updpkgsums
makepkg --printsrcinfo > .SRCINFO
```

Each published AUR package has its own Git repository. Copy the verified
`PKGBUILD` and `.SRCINFO` into clones of
`ssh://aur@aur.archlinux.org/kanban4ai.git` and
`ssh://aur@aur.archlinux.org/kanban4ai-bin.git`, then commit and push those
repositories separately.

For the VCS package, regenerate `.SRCINFO` after material package changes:

```sh
cd packaging/aur/kanban4ai-git
makepkg --printsrcinfo > .SRCINFO
```
