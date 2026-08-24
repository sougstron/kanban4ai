# kanban4ai 0.5.1

## Highlights

- Per-task designer and reviewer opt-in. A task can run the project
  designer or reviewer bot without turning that bot on for the whole
  board. Create/edit dialogs add Designer and Reviewer checkboxes under
  Interactive; `kanban create` grows `--designer` / `--reviewer`. Models
  and agents still come from `orchestration.designer` /
  `orchestration.reviewer`. `use_designer` / `use_reviewer` are omitted
  from frontmatter while false, so golden fixtures stay byte-identical.
- Review-edits wrap like the task Description (`WrapMode::WordOrGlyph`).
  The stored editor is rendered in place so wrap width and visual
  Up/Down stay correct after resize.
- Word delete works in both text fields: Description now honors
  Ctrl+Delete / Ctrl+Backspace, and Review-edits again honors
  Ctrl/Alt+Delete after the wrap change.

## Verification coverage

- Operations / scheduler: per-task `use_designer` / `use_reviewer` OR
  with the project switches; create flags persist; golden fixtures still
  byte-identical when the new fields are unset.
- TUI: Designer/Reviewer checkboxes on create/edit; review-edits wrap
  and word-delete; Description word-delete.
- Release checks for this version include rustfmt, clippy with warnings
  denied, locked tests, a release build, and installer packaging smoke
  tests.
