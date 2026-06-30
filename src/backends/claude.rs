//! Claude Code backend: reads the per-session `.jsonl` transcripts under
//! `~/.claude/projects/` (and every claudex account).
//!
//! This is a thin adapter — the heavy lifting (JSONL record decoding, session
//! discovery, worktree expansion, memory-file walking) lives in [`crate::parser`],
//! [`crate::sessions`], and [`crate::memory`]. This module just presents those
//! existing capabilities as a [`Source`].

use std::collections::HashSet;
use std::io::{BufRead, BufReader, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use crate::memory::{discover_memory_files, MemoryFile};
use crate::parser::{collect_tool_use_ids, extract_content_opts, extract_from_entry, ExtractedContent, Target, ToolUseMap};
use crate::sessions::{discover_projects, discover_sessions_with_worktrees, CLAUDE};
use crate::source::Source;
use crate::sessions::ProjectInfo;
use crate::sessions::SessionFile;

/// The Claude Code transcript store. `config_dirs` carries the default
/// `~/.claude` plus every claudex account root, so multi-account setups are
/// searched in one go.
pub struct ClaudeSource {
    pub config_dirs: Vec<(Option<String>, PathBuf)>,
}

impl ClaudeSource {
    pub fn new(config_dirs: Vec<(Option<String>, PathBuf)>) -> Self {
        Self { config_dirs }
    }

    fn config_dir_paths(&self) -> Vec<&Path> {
        self.config_dirs.iter().map(|(_, d)| d.as_path()).collect()
    }
}

impl Source for ClaudeSource {
    fn name(&self) -> &'static str {
        CLAUDE
    }

    fn discover_projects(&self) -> Vec<ProjectInfo> {
        discover_projects(&self.config_dirs)
    }

    fn discover_sessions(&self, project_path: &str) -> Vec<SessionFile> {
        // Mirror the old `discover_sessions_across_configs`: union every config
        // dir's sessions (each expanding git worktrees), dedup by file path.
        let mut seen_paths = std::collections::HashSet::new();
        let mut all = vec![];
        for (_, config_dir) in &self.config_dirs {
            for s in discover_sessions_with_worktrees(project_path, config_dir) {
                if seen_paths.insert(s.file_path.to_string_lossy().to_string()) {
                    all.push(s);
                }
            }
        }
        all.sort_by(|a, b| b.mtime.cmp(&a.mtime));
        all
    }

    fn extract_content(
        &self,
        session: &SessionFile,
        targets: &HashSet<Target>,
        keep_raw: bool,
    ) -> Vec<ExtractedContent> {
        extract_content_opts(
            &session.file_path,
            targets,
            &session.session_id,
            session.is_subagent,
            keep_raw,
        )
    }

    fn follow(
        &self,
        session: &SessionFile,
        targets: &HashSet<Target>,
        on_records: &mut dyn FnMut(&[ExtractedContent]),
    ) -> Result<(), String> {
        let mut file = std::fs::File::open(&session.file_path)
            .map_err(|e| format!("failed to open {}: {}", session.file_path.display(), e))?;

        // Seek to end — callers have already printed the initial tail.
        file.seek(SeekFrom::End(0)).map_err(|e| e.to_string())?;

        let mut tool_use_map = ToolUseMap::new();
        let mut reader = BufReader::new(file);
        let mut line_buf = String::new();

        loop {
            line_buf.clear();
            match reader.read_line(&mut line_buf) {
                Ok(0) => {
                    std::thread::sleep(std::time::Duration::from_millis(200));
                    continue;
                }
                Ok(_) => {
                    let line = line_buf.trim_end();
                    if line.is_empty() {
                        continue;
                    }
                    match serde_json::from_str::<serde_json::Value>(line) {
                        Ok(entry) => {
                            collect_tool_use_ids(&entry, &mut tool_use_map);
                            let mut results = vec![];
                            extract_from_entry(
                                &entry,
                                &tool_use_map,
                                targets,
                                &session.session_id,
                                session.is_subagent,
                                &mut results,
                            );
                            if !results.is_empty() {
                                on_records(&results);
                            }
                            let _ = std::io::stdout().flush();
                        }
                        Err(_) => {
                            // Likely an incomplete line (file mid-write): put the
                            // bytes back and re-read after a pause.
                            let n = line_buf.len() as i64;
                            let inner = reader.get_mut();
                            let _ = inner.seek(SeekFrom::Current(-n));
                            reader = BufReader::new(reader.into_inner());
                            std::thread::sleep(std::time::Duration::from_millis(200));
                        }
                    }
                }
                Err(_) => {
                    std::thread::sleep(std::time::Duration::from_millis(200));
                }
            }
        }
    }

    fn discover_memory_files(&self, cwd: &Path, include_subdirs: bool) -> Vec<MemoryFile> {
        let paths = self.config_dir_paths();
        discover_memory_files(cwd, &paths, include_subdirs)
    }
}
