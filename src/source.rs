//! The backend abstraction.
//!
//! Everything in claugrep downstream of [`ExtractedContent`] is format-agnostic:
//! searching, context-gathering, rendering, date/subagent filtering. The only
//! things a transcript store has to provide are:
//!
//!   1. which projects / sessions it knows about,
//!   2. the normalized content records of a session,
//!   3. a live-follow primitive (for `tail -f`), and
//!   4. the memory / instructions files that apply to a directory.
//!
//! Those four capabilities are the [`Source`] trait. Each backend (Claude's
//! JSONL files, opencode's SQLite DB, and future ones — codex, …) is one
//! self-contained `impl Source`. [`MultiSource`] composes any number of them
//! behind a single `&dyn Source`, merging discovery and dispatching per-session
//! operations to the owning backend via the `SessionFile::backend` tag. That
//! lets the rest of the program stay completely unaware that more than one
//! store exists.

use std::collections::{HashMap, HashSet};
use std::path::Path;

use crate::memory::MemoryFile;
use crate::parser::{ExtractedContent, Target};
use crate::sessions::{ProjectInfo, SessionFile};

/// A transcript backend.
///
/// Implementations are expected to be cheap to clone or to be held behind a
/// single shared reference; `MultiSource` dispatches by the `name()` each impl
/// reports, which is also stamped onto every `SessionFile` / `ProjectInfo` it
/// produces.
pub trait Source {
    /// Stable identifier for this backend (`"claude"`, `"opencode"`, …). Used as
    /// the dispatch key and as the `backend` tag on discovered sessions.
    fn name(&self) -> &'static str;

    /// List every known project for this source, most-recently-active first.
    fn discover_projects(&self) -> Vec<ProjectInfo>;

    /// Discover sessions whose project matches `project_path` (a canonical
    /// absolute path). Backends expand git worktrees themselves so the caller
    /// can hand over a single resolved path. Newest first.
    fn discover_sessions(&self, project_path: &str) -> Vec<SessionFile>;

    /// Extract normalized content records for one session. `keep_raw` asks the
    /// backend to preserve the raw record (as the backend stores it) on each
    /// `ExtractedContent::raw_entry` for `--json` output. Backends should target
    /// `targets` to skip irrelevant records early, but callers re-apply the
    /// full `TargetSelector` (subtypes included) afterwards.
    fn extract_content(
        &self,
        session: &SessionFile,
        targets: &HashSet<Target>,
        keep_raw: bool,
    ) -> Vec<ExtractedContent>;

    /// Follow a live session, invoking `on_records` with each batch of newly
    /// arrived content. Blocks until interrupted (the caller installs the
    /// signal handler). The closure receives records already extracted and
    /// target-filtered by the backend's extraction path; the caller applies its
    /// own `TargetSelector` subtype filtering on top.
    fn follow(
        &self,
        session: &SessionFile,
        targets: &HashSet<Target>,
        on_records: &mut dyn FnMut(&[ExtractedContent]),
    ) -> Result<(), String>;

    /// Memory / instructions files available to this backend for `cwd`
    /// (CLAUDE.md, AGENTS.md, native memory stores, …), in load order.
    fn discover_memory_files(&self, cwd: &Path, include_subdirs: bool) -> Vec<MemoryFile>;
}

/// A [`Source`] composed of several backends. Discovery merges across all
/// children; per-session operations (`extract_content`, `follow`) are routed to
/// the child whose `name()` matches the session's `backend` tag.
///
/// Construct with [`MultiSource::new`]; pass `&multi` as `&dyn Source` to the
/// rest of the program.
pub struct MultiSource {
    children: Vec<Box<dyn Source>>,
    /// `name()` → index into `children`, for per-session dispatch.
    by_name: HashMap<&'static str, usize>,
}

impl MultiSource {
    pub fn new(children: Vec<Box<dyn Source>>) -> Self {
        let by_name = children
            .iter()
            .enumerate()
            .map(|(i, c)| (c.name(), i))
            .collect();
        // If two children share a name the later wins silently; that would be a
        // programming error (duplicate backend) so we don't try to recover.
        Self { children, by_name }
    }

    #[allow(dead_code)]
    pub fn is_empty(&self) -> bool {
        self.children.is_empty()
    }

    #[allow(dead_code)]
    pub fn names(&self) -> Vec<&'static str> {
        self.children.iter().map(|c| c.name()).collect()
    }

    /// The child that owns sessions tagged `backend`, if any.
    fn child_for(&self, backend: &str) -> Option<&dyn Source> {
        self.by_name.get(backend).map(|&i| self.children[i].as_ref())
    }
}

impl Source for MultiSource {
    fn name(&self) -> &'static str {
        "multi"
    }

    fn discover_projects(&self) -> Vec<ProjectInfo> {
        let mut all: Vec<ProjectInfo> = self
            .children
            .iter()
            .flat_map(|c| c.discover_projects())
            .collect();
        // Most-recently-active across all backends first.
        all.sort_by(|a, b| {
            let ta = a.latest_mtime.unwrap_or(std::time::UNIX_EPOCH);
            let tb = b.latest_mtime.unwrap_or(std::time::UNIX_EPOCH);
            tb.cmp(&ta)
        });
        all
    }

    fn discover_sessions(&self, project_path: &str) -> Vec<SessionFile> {
        // Each child returns its sessions newest-first; interleave by mtime so
        // the merged stream is also newest-first (offset-based session selection
        // like `--session 0` relies on this).
        let mut all: Vec<SessionFile> = self
            .children
            .iter()
            .flat_map(|c| c.discover_sessions(project_path))
            .collect();
        all.sort_by(|a, b| b.mtime.cmp(&a.mtime));
        all
    }

    fn extract_content(
        &self,
        session: &SessionFile,
        targets: &HashSet<Target>,
        keep_raw: bool,
    ) -> Vec<ExtractedContent> {
        match self.child_for(session.backend) {
            Some(child) => child.extract_content(session, targets, keep_raw),
            None => {
                eprintln!(
                    "warning: no source registered for backend '{}'; skipping session {}",
                    session.backend, session.session_id
                );
                Vec::new()
            }
        }
    }

    fn follow(
        &self,
        session: &SessionFile,
        targets: &HashSet<Target>,
        on_records: &mut dyn FnMut(&[ExtractedContent]),
    ) -> Result<(), String> {
        match self.child_for(session.backend) {
            Some(child) => child.follow(session, targets, on_records),
            None => Err(format!(
                "no source registered for backend '{}'",
                session.backend
            )),
        }
    }

    fn discover_memory_files(&self, cwd: &Path, include_subdirs: bool) -> Vec<MemoryFile> {
        // Order: preserve child order (callers construct Claude first, then
        // opencode, …). dedup by path so a backend that happens to share a file
        // doesn't double-list it.
        let mut seen: std::collections::HashSet<std::path::PathBuf> = std::collections::HashSet::new();
        let mut out: Vec<MemoryFile> = Vec::new();
        for c in &self.children {
            for f in c.discover_memory_files(cwd, include_subdirs) {
                if seen.insert(f.path.clone()) {
                    out.push(f);
                }
            }
        }
        out
    }
}
