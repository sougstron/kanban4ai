//! Rule-based context compaction. NO LLM calls — the rules keep headings,
//! code fences, and error lines, and drop consecutive duplicates and runs of
//! progress chatter.

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use regex::Regex;

use crate::core::context::ContextManager;
use crate::core::error::Result;
use crate::core::storage::Storage;
use crate::core::timefmt;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CompactionStatus {
    NoContext,
    BelowThreshold,
    Compacted,
}

#[derive(Debug, Clone)]
pub struct CompactionResult {
    pub status: CompactionStatus,
    pub before: usize,
    pub after: usize,
}

impl CompactionResult {
    pub fn reduction(&self) -> usize {
        self.before.saturating_sub(self.after)
    }
}

pub struct CompactionManager {
    pub project_path: PathBuf,
    pub context_dir: PathBuf,
    pub compaction_log: PathBuf,
}

impl CompactionManager {
    pub fn new(project_path: impl AsRef<Path>) -> Self {
        let project_path = project_path.as_ref().to_path_buf();
        let kanban_dir = project_path.join(".kanban");
        CompactionManager {
            context_dir: kanban_dir.join("context"),
            compaction_log: kanban_dir.join("compaction.log"),
            project_path,
        }
    }

    pub fn compact_context(
        &self,
        task_id: &str,
        threshold: usize,
        force: bool,
    ) -> Result<CompactionResult> {
        let ctx_mgr = ContextManager::new(&self.project_path);
        let storage = Storage::new(&self.project_path);

        let context = ctx_mgr.get_context(task_id, &storage)?;
        if context.is_empty() {
            return Ok(CompactionResult {
                status: CompactionStatus::NoContext,
                before: 0,
                after: 0,
            });
        }

        let size = context.len();
        if !force && size < threshold {
            return Ok(CompactionResult {
                status: CompactionStatus::BelowThreshold,
                before: size,
                after: size,
            });
        }

        let compacted = compact_text(&context);
        let after_size = compacted.len();

        // Legacy spill file: keep a .bak and rewrite in place.
        let context_file = self.context_dir.join(format!("{task_id}.md"));
        if context_file.exists() {
            let backup_file = self.context_dir.join(format!("{task_id}.md.bak"));
            let original = fs::read_to_string(&context_file)?;
            fs::write(&backup_file, original)?;
            fs::write(&context_file, &compacted)?;
        }

        self.log_compaction(task_id, size, after_size);

        Ok(CompactionResult {
            status: CompactionStatus::Compacted,
            before: size,
            after: after_size,
        })
    }

    fn log_compaction(&self, task_id: &str, before: usize, after: usize) {
        if let Ok(mut file) = fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.compaction_log)
        {
            let _ = writeln!(
                file,
                "{} - {}: {} -> {} bytes",
                timefmt::format(&timefmt::now()),
                task_id,
                before,
                after
            );
        }
    }
}

pub fn compact_text(text: &str) -> String {
    let mut result: Vec<&str> = Vec::new();
    let mut prev_line: Option<&str> = None;

    for line in text.split('\n') {
        let stripped = line.trim();

        if stripped.starts_with("```")
            || stripped.starts_with("## ")
            || stripped.starts_with("### ")
            || stripped.starts_with("Error:")
            || stripped.contains("Traceback")
        {
            result.push(line);
            prev_line = Some(line);
            continue;
        }

        if Some(stripped) == prev_line {
            continue;
        }

        // Collapse runs of progress chatter to their first line.
        if (stripped.starts_with("Working on") || stripped.starts_with("Checking"))
            && prev_line.is_some_and(|prev| {
                let prev = prev.trim();
                prev.starts_with("Working on") || prev.starts_with("Checking")
            })
        {
            continue;
        }

        result.push(line);
        prev_line = Some(stripped);
    }

    let compacted = result.join("\n");
    let squeezed = Regex::new(r"\n{3,}")
        .expect("static regex")
        .replace_all(&compacted, "\n\n")
        .into_owned();
    squeezed.trim().to_string()
}
