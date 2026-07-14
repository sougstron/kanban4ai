---
id: TASK-004
title: UI fixes
status: archive
session: null
created_at: '2026-06-01T08:50:45.631884'
updated_at: '2026-06-07T16:49:03.060363'
priority: low
tags: []
has_questions: false
context_file: null
context_size: 459
ai_model: null
---
Issue 1: When I press enter to delete the task I cannot select anything with my keyboard (arrows). Need to fix it.
Issue 2: In review column we need a button to mark task as done and move it to that column.
Issue 3: Save theme when it's selected for project where it selected
Issue 4: Dublicate hotkeys to russian keyboard layout so doesn't mean on which lang I will press the hotkeys on my keyboard

## Context

## Context Entry - 2026-06-01T18:23:49.661426 (agent)
Implemented TUI fixes: delete confirmation now focuses buttons and supports arrow/Enter selection; review task detail includes Mark Done action; selected Textual theme persists in .kanban/config.yaml per project; main/detail hotkeys now support Russian layout equivalents. Validation: lsp_diagnostics clean on changed files, pytest 59 passed, TUI screenshot saved to .sisyphus/evidence/TASK-004-tui.svg.
