# kanban4ai 0.3.1

## Highlights

- First-class `omp` and `pi` agent backends: non-interactive launch with `-p`,
  model selection, and `--thinking` effort, alongside the existing opencode and
  claude backends.
- Live model catalogs for omp (`omp models --json`) and pi (on-disk
  `models-store.json`), with the same default/recent/alphabetical ordering used
  for opencode in the TUI create/edit dialogs.
- Catalog warming and recent-model history now cover every catalog backend
  (opencode, omp, pi), not only opencode.
- Default board config ships omp/pi backend entries; `kanban create` accepts
  `--backend omp|pi`.

## Verification coverage

- Launch-plan coverage for omp/pi (`-p`, `--model`, `--thinking`).
- Parsers for `omp models --json` and pi `models-store.json`.
- Release checks for this version include rustfmt, clippy with warnings denied,
  locked tests, a release build, and installer packaging smoke tests.
