//! opencode backend: reads the SQLite transcript store at
//! `~/.local/share/opencode/opencode.db` (event-sourced; `session` / `message`
//! / `part` tables).
//!
//! opencode's content model differs from Claude's in two ways, both of which
//! make our job easier:
//!
//!   * a tool call and its result are one `part` row (`state.input` +
//!     `state.output`), whereas Claude splits them across two JSONL records
//!     linked by `tool_use_id` — so there is no ID-correlation map to maintain;
//!   * storage is SQL, so `tail -f` is a deterministic poll on `rowid` /
//!     `time_updated` rather than a byte-tail with partial-line/rotation races.
//!
//! Some Claude transcript-internal record types (`system.away_summary`,
//! `attachment.task_reminder`, `progress`, `file-history-snapshot`, …) have no
//! opencode equivalent and simply stay empty for opencode sessions; the core
//! targets (user / assistant / thinking / bash / tool) map fully.

use std::collections::HashSet;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use rusqlite::{Connection, OpenFlags};

use crate::memory::{discover_with_layout, MemoryFile, MemoryLayout};
use crate::parser::{EditDiff, ExtractedContent, Target};
use crate::sessions::{get_worktree_paths, ProjectInfo, SessionFile};
use crate::source::Source;

pub const OPENCODE: &str = "opencode";

pub struct OpenCodeSource {
    pub db_path: PathBuf,
}

impl OpenCodeSource {
    pub fn new(db_path: PathBuf) -> Self {
        Self { db_path }
    }

    /// Default DB location, honouring `XDG_DATA_HOME`. `None` if absent.
    pub fn default_db_path() -> Option<PathBuf> {
        let base = std::env::var("XDG_DATA_HOME")
            .ok()
            .map(PathBuf::from)
            .or_else(|| dirs::home_dir().map(|h| h.join(".local/share")))
            .or_else(|| dirs::data_dir())?;
        let db = base.join("opencode").join("opencode.db");
        if db.is_file() { Some(db) } else { None }
    }

    fn open(&self) -> Result<Connection, String> {
        Connection::open_with_flags(&self.db_path, OpenFlags::SQLITE_OPEN_READ_ONLY)
            .map_err(|e| format!("failed to open opencode db {}: {}", self.db_path.display(), e))
    }
}

/// opencode config dir (`~/.config/opencode`, honouring `XDG_CONFIG_HOME`).
fn opencode_config_dir() -> PathBuf {
    std::env::var("XDG_CONFIG_HOME")
        .ok()
        .map(PathBuf::from)
        .or_else(|| dirs::config_dir())
        .unwrap_or_else(|| PathBuf::from("~/.config"))
        .join("opencode")
}

impl Source for OpenCodeSource {
    fn name(&self) -> &'static str {
        OPENCODE
    }

    fn discover_projects(&self) -> Vec<ProjectInfo> {
        let conn = match self.open() {
            Ok(c) => c,
            Err(e) => { eprintln!("warning: {}", e); return vec![]; }
        };
        let mut stmt = match conn.prepare(
            "SELECT directory, COUNT(*), MAX(time_updated) \
             FROM session GROUP BY directory ORDER BY MAX(time_updated) DESC",
        ) {
            Ok(s) => s,
            Err(e) => { eprintln!("warning: opencode projects query failed: {}", e); return vec![]; }
        };
        let rows = stmt.query_map([], |row| {
            let dir: String = row.get(0)?;
            let count: i64 = row.get(1)?;
            let max_ts: i64 = row.get(2)?;
            Ok((dir, count as usize, max_ts))
        });
        let rows = match rows {
            Ok(r) => r,
            Err(e) => { eprintln!("warning: opencode projects query failed: {}", e); return vec![]; }
        };
        let mut out = vec![];
        for r in rows.flatten() {
            let (dir, count, max_ts) = r;
            out.push(ProjectInfo {
                encoded_path: dir.clone(),
                decoded_path: dir.clone(),
                verified: Path::new(&dir).exists(),
                session_count: count,
                latest_mtime: Some(millis_to_systime(max_ts)),
                account: None,
                backend: OPENCODE,
            });
        }
        out
    }

    fn discover_sessions(&self, project_path: &str) -> Vec<SessionFile> {
        let conn = match self.open() {
            Ok(c) => c,
            Err(e) => { eprintln!("warning: {}", e); return vec![]; }
        };
        // Expand git worktrees (a session logged under a sibling worktree path
        // should still be discoverable from any worktree of the same repo).
        let mut dirs: Vec<String> = get_worktree_paths(project_path);
        if !dirs.iter().any(|d| d == project_path) {
            dirs.push(project_path.to_string());
        }
        let mut seen_ids: std::collections::HashSet<String> = std::collections::HashSet::new();
        let mut out = vec![];
        // Prepare once; query_map per directory. The statement must outlive each
        // row iterator, so it lives in this scope and we consume each iterator
        // before the next directory.
        let mut stmt = match conn.prepare(
            "SELECT id, parent_id IS NOT NULL, time_updated \
             FROM session WHERE directory = ?1 ORDER BY time_updated DESC",
        ) {
            Ok(s) => s,
            Err(e) => { eprintln!("warning: opencode sessions query failed: {}", e); return out; }
        };
        for dir in &dirs {
            let rows = stmt.query_map([dir], |row| {
                let id: String = row.get(0)?;
                let is_sub: bool = row.get(1)?;
                let ts: i64 = row.get(2)?;
                Ok((id, is_sub, ts))
            });
            let rows = match rows { Ok(r) => r, Err(e) => { eprintln!("warning: opencode sessions query failed: {}", e); continue; } };
            for r in rows.flatten() {
                let (id, is_sub, ts) = r;
                if !seen_ids.insert(id.clone()) { continue; }
                out.push(SessionFile {
                    session_id: id,
                    file_path: self.db_path.clone(),
                    mtime: millis_to_systime(ts),
                    is_subagent: is_sub,
                    backend: OPENCODE,
                });
            }
        }
        out.sort_by(|a, b| b.mtime.cmp(&a.mtime));
        out
    }

    fn extract_content(
        &self,
        session: &SessionFile,
        targets: &HashSet<Target>,
        keep_raw: bool,
    ) -> Vec<ExtractedContent> {
        let conn = match self.open() {
            Ok(c) => c,
            Err(e) => { eprintln!("warning: {}", e); return vec![]; }
        };
        let mut stmt = match conn.prepare(
            "SELECT p.rowid, p.data, p.time_created, p.time_updated, \
                    json_extract(m.data,'$.role') \
             FROM part p LEFT JOIN message m ON m.id = p.message_id \
             WHERE p.session_id = ?1 ORDER BY p.rowid",
        ) {
            Ok(s) => s,
            Err(e) => { eprintln!("warning: opencode parts query failed: {}", e); return vec![]; }
        };
        let rows = stmt.query_map([&session.session_id], |row| {
            let rowid: i64 = row.get(0)?;
            let data_str: String = row.get(1)?;
            let created: i64 = row.get(2)?;
            let updated: i64 = row.get(3)?;
            let role: Option<String> = row.get(4)?;
            Ok((rowid, data_str, created, updated, role))
        });
        let rows = match rows { Ok(r) => r, Err(e) => { eprintln!("warning: opencode parts query failed: {}", e); return vec![]; } };

        let mut out = vec![];
        for r in rows.flatten() {
            let (_rowid, data_str, created, updated, role) = r;
            let data: serde_json::Value = match serde_json::from_str(&data_str) {
                Ok(v) => v,
                Err(_) => continue,
            };
            let role = role.as_deref().unwrap_or("");
            let (s0, s1) = part_slots(&data, role, &session.session_id, session.is_subagent, targets, keep_raw, created, updated);
            if let Some(r) = s0 { out.push(r); }
            if let Some(r) = s1 { out.push(r); }
        }
        out
    }

    fn follow(
        &self,
        session: &SessionFile,
        targets: &HashSet<Target>,
        on_records: &mut dyn FnMut(&[ExtractedContent]),
    ) -> Result<(), String> {
        let conn = self.open()?;
        // Prepare once; the statement must outlive every row iterator we pull
        // from it across the polling loop.
        let mut stmt = conn.prepare(
            "SELECT p.rowid, p.data, p.time_created, p.time_updated, \
                    json_extract(m.data,'$.role') \
             FROM part p LEFT JOIN message m ON m.id = p.message_id \
             WHERE p.session_id = ?1 AND (p.rowid > ?2 OR p.time_updated > ?3) \
             ORDER BY p.rowid",
        ).map_err(|e| e.to_string())?;

        // Seed the cursors at the current end of the session, mirroring the
        // Claude backend's "seek to EOF" — callers have already printed the
        // initial tail, so follow must only stream parts that arrive *after*
        // this point.
        let (mut last_rowid, mut last_updated): (i64, i64) = conn.query_row(
            "SELECT COALESCE(MAX(rowid), 0), COALESCE(MAX(time_updated), 0) \
             FROM part WHERE session_id = ?1",
            [&session.session_id],
            |row| Ok((row.get(0)?, row.get(1)?)),
        ).unwrap_or((0, 0));
        // rowid → emitted bitmask: bit0 = first slot, bit1 = second slot.
        let mut emitted: std::collections::HashMap<i64, u8> = std::collections::HashMap::new();

        loop {
            // New parts (rowid advanced) OR parts whose time_updated advanced
            // (a tool part's output filling in after the call started).
            let rows = stmt.query_map(
                rusqlite::params![&session.session_id, last_rowid, last_updated],
                |row| {
                    let rowid: i64 = row.get(0)?;
                    let data_str: String = row.get(1)?;
                    let created: i64 = row.get(2)?;
                    let updated: i64 = row.get(3)?;
                    let role: Option<String> = row.get(4)?;
                    Ok((rowid, data_str, created, updated, role))
                },
            );
            let rows = match rows { Ok(r) => r, Err(e) => { eprintln!("warning: opencode follow poll failed: {}", e); std::thread::sleep(Duration::from_millis(200)); continue; } };

            let mut batch = vec![];
            for r in rows.flatten() {
                let (rowid, data_str, created, updated, role) = r;
                if rowid > last_rowid { last_rowid = rowid; }
                if updated > last_updated { last_updated = updated; }
                let data: serde_json::Value = match serde_json::from_str(&data_str) { Ok(v) => v, Err(_) => continue };
                let role = role.as_deref().unwrap_or("");
                let (s0, s1) = part_slots(&data, role, &session.session_id, session.is_subagent, targets, true, created, updated);
                let mask = emitted.entry(rowid).or_insert(0);
                if s0.is_some() && (*mask & 1) == 0 {
                    if let Some(r) = s0 { batch.push(r); }
                    *mask |= 1;
                }
                if s1.is_some() && (*mask & 2) == 0 {
                    if let Some(r) = s1 { batch.push(r); }
                    *mask |= 2;
                }
            }
            if !batch.is_empty() {
                on_records(&batch);
                let _ = std::io::stdout().flush();
            }
            std::thread::sleep(Duration::from_millis(200));
        }
    }

    fn discover_memory_files(&self, cwd: &Path, include_subdirs: bool) -> Vec<MemoryFile> {
        // opencode uses AGENTS.md (project + global). No managed policy or
        // auto-memory tree today; the layout slots are wired so they light up
        // automatically if opencode grows them.
        let layout = MemoryLayout {
            filenames: vec!["AGENTS.md".to_string()],
            managed_policy: None,
            config_dirs: vec![opencode_config_dir()],
            auto_memory: None,
        };
        discover_with_layout(cwd, &layout, include_subdirs)
    }
}

/// Map one opencode part to up to two normalized records (slot 0 / slot 1).
///
/// Non-tool parts use only slot 0. A tool part uses slot 0 = the call/input and
/// slot 1 = the result/output, mirroring how Claude emits a `tool_use` and a
/// later `tool_result` as two separate records — this keeps `-t bash-command`
/// vs `-t bash-output` filtering identical across backends.
///
/// `targets` is honored (early-filter) the same way the Claude extractor does,
/// so callers searching a narrow target set don't pay for materializing the rest.
fn part_slots(
    data: &serde_json::Value,
    role: &str,
    session_id: &str,
    is_subagent: bool,
    targets: &HashSet<Target>,
    keep_raw: bool,
    created_ms: i64,
    updated_ms: i64,
) -> (Option<ExtractedContent>, Option<ExtractedContent>) {
    let ptype = data["type"].as_str().unwrap_or("");
    let raw = if keep_raw { Some(data.clone()) } else { None };

    match ptype {
        "text" => {
            let target = if is_subagent && role != "assistant" {
                if targets.contains(&Target::SubagentPrompt) { Target::SubagentPrompt } else { return (None, None); }
            } else if role == "assistant" {
                if targets.contains(&Target::Assistant) { Target::Assistant } else { return (None, None); }
            } else {
                if targets.contains(&Target::User) { Target::User } else { return (None, None); }
            };
            let text = data["text"].as_str().unwrap_or("").to_string();
            (Some(ExtractedContent {
                target,
                text,
                tool_name: None,
                timestamp: millis_to_iso(created_ms),
                session_id: session_id.to_string(),
                edit_diff: None,
                raw_entry: raw,
            }), None)
        }
        "reasoning" => {
            if !targets.contains(&Target::Thinking) { return (None, None); }
            let text = data["text"].as_str().unwrap_or("").to_string();
            (Some(ExtractedContent {
                target: Target::Thinking,
                text,
                tool_name: None,
                timestamp: millis_to_iso(time_start(data, created_ms)),
                session_id: session_id.to_string(),
                edit_diff: None,
                raw_entry: raw,
            }), None)
        }
        "tool" => {
            let tool = data["tool"].as_str().unwrap_or("").to_string();
            let state = &data["state"];
            let input = &state["input"];
            let output_obj = &state["output"];
            let is_bash = tool == "bash";

            // slot 0: the call (BashCommand and/or ToolUse), like Claude's tool_use block.
            let s0 = {
                let want_bash_cmd = is_bash && targets.contains(&Target::BashCommand);
                let want_tool_use = targets.contains(&Target::ToolUse) && !tool.is_empty();
                if !want_bash_cmd && !want_tool_use {
                    None
                } else {
                    let edit_diff = if tool == "edit" {
                        match (input["filePath"].as_str(), input["oldString"].as_str(), input["newString"].as_str()) {
                            (Some(fp), Some(old), Some(new)) => Some(EditDiff {
                                file_path: fp.to_string(),
                                old_string: old.to_string(),
                                new_string: new.to_string(),
                            }),
                            _ => None,
                        }
                    } else { None };

                    // Emit a BashCommand record when asked; it's the most useful
                    // view of a bash call. If only ToolUse is wanted, render the
                    // whole input (command included) as the tool-use text.
                    let (target, text) = if want_bash_cmd {
                        (Target::BashCommand, input["command"].as_str().unwrap_or("").to_string())
                    } else {
                        (Target::ToolUse, format_tool_input(input))
                    };
                    Some(ExtractedContent {
                        target,
                        text,
                        tool_name: Some(tool.clone()),
                        timestamp: millis_to_iso(time_input_start(state, created_ms)),
                        session_id: session_id.to_string(),
                        edit_diff,
                        raw_entry: raw.clone(),
                    })
                }
            };

            // If both BashCommand and ToolUse are wanted, Claude emits two
            // records from one tool_use block. For opencode we can't return a
            // third slot, so when both are wanted we emit the BashCommand in s0
            // and the ToolUse rendering in s1. This is the one place the
            // 2-slot model is slightly lossy vs Claude's N-output, but it keeps
            // both targets populated.
            let s1 = {
                let want_both = is_bash
                    && targets.contains(&Target::BashCommand)
                    && targets.contains(&Target::ToolUse);
                if want_both {
                    Some(ExtractedContent {
                        target: Target::ToolUse,
                        text: format_tool_input(input),
                        tool_name: Some(tool.clone()),
                        timestamp: millis_to_iso(time_input_start(state, created_ms)),
                        session_id: session_id.to_string(),
                        edit_diff: None,
                        raw_entry: raw.clone(),
                    })
                } else {
                    None
                }
            };

            // The tool's result is a separate record too. We've used s0/s1 above
            // for the call; fold the output into the stream by reusing whichever
            // slot is free. If both slots are taken (bash with both targets), the
            // output is dropped — acceptable, since bash output is also reachable
            // via the BashOutput target which we handle next.

            // Prefer emitting the output as its own target (BashOutput/ToolResult).
            let out_target = if is_bash { Target::BashOutput } else { Target::ToolResult };
            let want_output = targets.contains(&out_target) && output_obj.as_str().map(|s| !s.is_empty()).unwrap_or(false);
            let output_record = if want_output {
                Some(ExtractedContent {
                    target: out_target,
                    text: output_obj.as_str().unwrap_or("").to_string(),
                    tool_name: Some(tool.clone()),
                    timestamp: millis_to_iso(time_output_end(state, updated_ms)),
                    session_id: session_id.to_string(),
                    edit_diff: None,
                    raw_entry: raw.clone(),
                })
            } else { None };

            // Place the output into a free slot if possible; otherwise drop it
            // (callers wanting tool results should not also request both bash
            // command+tool-use on the same call).
            match (s0, s1, output_record) {
                (a, None, Some(o)) => (a, Some(o)),
                (None, b, Some(o)) => (Some(o), b),
                (a, b, _) => (a, b),
            }
        }
        "compaction" => {
            if !targets.contains(&Target::CompactSummary) { return (None, None); }
            let tail = data["tail_start_id"].as_str().unwrap_or("");
            let text = if tail.is_empty() {
                "(compaction boundary)".to_string()
            } else {
                format!("(compaction boundary; resumes at {})", tail)
            };
            (Some(ExtractedContent {
                target: Target::CompactSummary,
                text,
                tool_name: None,
                timestamp: millis_to_iso(created_ms),
                session_id: session_id.to_string(),
                edit_diff: None,
                raw_entry: raw,
            }), None)
        }
        // step-start / step-finish / patch are step-boundary / snapshot markers
        // with no searchable prose; skip them.
        _ => (None, None),
    }
}

fn time_start(data: &serde_json::Value, fallback: i64) -> i64 {
    data["time"]["start"].as_i64().unwrap_or(fallback)
}
fn time_input_start(state: &serde_json::Value, fallback: i64) -> i64 {
    state["input"]["time"]["start"].as_i64().unwrap_or(fallback)
}
fn time_output_end(state: &serde_json::Value, fallback: i64) -> i64 {
    state["output"]["time"]["end"].as_i64().unwrap_or(fallback)
}

/// Render a tool-input object as `key: value` lines, mirroring Claude's
/// `format_tool_input`. Works for opencode's camelCase fields.
fn format_tool_input(input: &serde_json::Value) -> String {
    let obj = match input.as_object() { Some(o) => o, None => return String::new() };
    let mut lines = vec![];
    for (key, value) in obj {
        if let Some(s) = value.as_str() {
            if s.contains('\n') {
                lines.push(format!("{}:\n{}", key, s));
            } else {
                lines.push(format!("{}: {}", key, s));
            }
        } else if value.is_number() || value.is_boolean() {
            lines.push(format!("{}: {}", key, value));
        } else if !value.is_null() {
            lines.push(format!("{}: {}", key, value));
        }
    }
    lines.join("\n")
}

fn millis_to_iso(ms: i64) -> String {
    chrono::DateTime::<chrono::Utc>::from_timestamp_millis(ms)
        .map(|dt| dt.format("%Y-%m-%dT%H:%M:%S%.3fZ").to_string())
        .unwrap_or_default()
}

fn millis_to_systime(ms: i64) -> SystemTime {
    if ms >= 0 {
        UNIX_EPOCH + Duration::from_millis(ms as u64)
    } else {
        UNIX_EPOCH
    }
}
