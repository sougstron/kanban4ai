# kanban4ai — план переписывания на Rust

Переписывание Python/Textual `kanban-cli` в нативное Rust-приложение
**kanban4ai** завершено. Все фазы 0–5 выполнены.

## Итоговые инварианты

1. Формат `.kanban/` совместим с существующими досками: Markdown + YAML
   frontmatter, статус определяется поддиректорией, треды хранятся в sidecar
   YAML с оптимистичным `rev`.
2. CLI-контракт для агентов (`kanban <cmd> … --agent`) сохранён.
3. Cargo собирает один бинарник `kanban4ai`; установщики создают относительные
   симлинки `kanban` и `kb`.
4. Все изменения данных атомарны, read-modify-write защищён board lock.
5. Golden-фикстуры в `tests/fixtures/` сохраняют совместимость формата досок.

## Завершённые фазы

- **Фаза 0 — каркас и фикстуры:** создан Rust crate, подготовлены golden-файлы.
- **Фаза 1 — ядро данных:** модели, конфиг, storage, thread merge, timestamp
  compatibility.
- **Фаза 2 — CLI и операции:** полный набор команд, правила агентов, вопросы,
  review edits, chaining, sessions и compaction.
- **Фаза 3 — запуск агентов:** opencode/Claude Code, tmux/background fallback,
  prompts, backups/revert, notifications и chained запуск.
- **Фаза 4 — TUI:** ratatui board, detail/dialogs, themes, search, sessions,
  image attachments и inotify refresh.
- **Фаза 5 — упаковка:** GitHub Actions CI и tag releases, Linux x86_64/aarch64
  archives with checksums, source AUR recipes `kanban4ai`/`kanban4ai-git`,
  POSIX installer, packaging smoke test, Cargo metadata, MIT license and final
  documentation. Python source removed; Rust tests and fixtures retained.

## Релизный процесс

1. Выполнить проверки из `README.md`, включая packaging smoke test.
2. Для стабильного AUR-рецепта заменить bootstrap `SKIP` реальным SHA-256
   source-архива и обновить `.SRCINFO` по `packaging/aur/README.md`.
3. Создать тег `v<version>` только после успешной проверки. Release workflow
   повторно запускает fmt, clippy, tests и release build, затем публикует
   Linux-архивы и `.sha256` файлы.
4. Live smoke tests opencode/Claude Code выполняются отдельно в безопасной
   тестовой доске; CI не требует внешних учётных данных.

## Принятые решения

- Usage overlay не переносился: он не работал надёжно.
- Компакция остаётся rule-based, без LLM.
- Приложение и релизные бинарники Unix-oriented; неподтверждённые Windows/macOS
  артефакты не заявляются.
- Канонический URL: <https://github.com/sougstron/kanban4ai>.
