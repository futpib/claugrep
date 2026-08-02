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
//! `attachment.task_reminder`, `progress`, `file-history-snapshot`,
//! `file-history-delta`, …) have no
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
            for (_slot, rec) in part_records(&data, role, &session.session_id, session.is_subagent, targets, keep_raw, created, updated) {
                out.push(rec);
            }
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
        // (rowid, slot) pairs already streamed — dedups per facet of a part so a
        // tool call's input and output each emit exactly once.
        let mut emitted: std::collections::HashSet<(i64, &'static str)> = std::collections::HashSet::new();

        loop {
            let batch = poll_follow_once(
                &mut stmt,
                &session.session_id,
                session.is_subagent,
                targets,
                &mut last_rowid,
                &mut last_updated,
                &mut emitted,
            );
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

/// One iteration of the follow poll loop, factored out so it is unit-testable
/// without the blocking/sleep. Advances `last_rowid` / `last_updated` and the
/// per-rowid `emitted` bitmask in place, and returns the newly-emitted records
/// (each slot at most once per rowid).
fn poll_follow_once(
    stmt: &mut rusqlite::Statement,
    session_id: &str,
    is_subagent: bool,
    targets: &HashSet<Target>,
    last_rowid: &mut i64,
    last_updated: &mut i64,
    emitted: &mut std::collections::HashSet<(i64, &'static str)>,
) -> Vec<ExtractedContent> {
    let rows = stmt.query_map(
        rusqlite::params![session_id, *last_rowid, *last_updated],
        |row| {
            let rowid: i64 = row.get(0)?;
            let data_str: String = row.get(1)?;
            let created: i64 = row.get(2)?;
            let updated: i64 = row.get(3)?;
            let role: Option<String> = row.get(4)?;
            Ok((rowid, data_str, created, updated, role))
        },
    );
    let mut batch = vec![];
    let rows = match rows {
        Ok(r) => r,
        Err(e) => { eprintln!("warning: opencode follow poll failed: {}", e); return batch; }
    };
    for r in rows.flatten() {
        let (rowid, data_str, created, updated, role) = r;
        if rowid > *last_rowid { *last_rowid = rowid; }
        if updated > *last_updated { *last_updated = updated; }
        let data: serde_json::Value = match serde_json::from_str(&data_str) { Ok(v) => v, Err(_) => continue };
        let role = role.as_deref().unwrap_or("");
        for (slot, rec) in part_records(&data, role, session_id, is_subagent, targets, true, created, updated) {
            if emitted.insert((rowid, slot)) {
                batch.push(rec);
            }
        }
    }
    batch
}

/// Map one opencode part to zero or more normalized records, each tagged with a
/// stable `slot` key (e.g. `"cmd"`, `"use"`, `"out"`).
///
/// A tool part can yield up to three records — the call as `BashCommand`, the
/// call as `ToolUse`, and the result as `BashOutput`/`ToolResult` — mirroring
/// how Claude emits a `tool_use` block (which may produce both a BashCommand and
/// a ToolUse record) plus a later `tool_result` record. This keeps `-t
/// bash-command` vs `-t bash-output` vs `-t tool-use` filtering identical across
/// backends, *and* lets a default search (which requests all of them) see every
/// facet of the call.
///
/// The `slot` keys let the follow loop dedup per-rowid without re-deriving
/// equality from record contents.
///
/// `targets` is honored (early-filter) the same way the Claude extractor does,
/// so callers searching a narrow target set don't pay for materializing the rest.
fn part_records(
    data: &serde_json::Value,
    role: &str,
    session_id: &str,
    is_subagent: bool,
    targets: &HashSet<Target>,
    keep_raw: bool,
    created_ms: i64,
    updated_ms: i64,
) -> Vec<(&'static str, ExtractedContent)> {
    let ptype = data["type"].as_str().unwrap_or("");

    // For `--json`, emit a NORMALIZED envelope (not the bare part) so the
    // cross-backend keys consumers reach for actually exist: `sessionId`,
    // `timestamp`, `type` (normalized to the message role, matching Claude's
    // record-level `type`), plus `partType` (the native opencode part type) and
    // `slot` (which facet of the part this record is). All native part fields
    // stay at top level, so nothing is lost. Deep content structure
    // (`message.content[]` vs flat part fields) remains backend-native — that
    // difference is irreducible — but the envelope ports.
    let base: Option<serde_json::Value> = if keep_raw {
        let mut env = data.clone();
        env["sessionId"] = serde_json::Value::String(session_id.to_string());
        env["role"] = serde_json::Value::String(role.to_string());
        env["partType"] = serde_json::Value::String(ptype.to_string());
        env["type"] = serde_json::Value::String(role.to_string());
        Some(env)
    } else { None };
    let finalize = |slot: &'static str, ts_iso: &str| -> Option<serde_json::Value> {
        base.as_ref().map(|b| {
            let mut v = b.clone();
            v["timestamp"] = serde_json::Value::String(ts_iso.to_string());
            v["slot"] = serde_json::Value::String(slot.to_string());
            v
        })
    };

    let mut out: Vec<(&'static str, ExtractedContent)> = vec![];

    match ptype {
        "text" => {
            let target = if is_subagent && role != "assistant" {
                if targets.contains(&Target::SubagentPrompt) { Target::SubagentPrompt } else { return out; }
            } else if role == "assistant" {
                if targets.contains(&Target::Assistant) { Target::Assistant } else { return out; }
            } else {
                if targets.contains(&Target::User) { Target::User } else { return out; }
            };
            let ts = millis_to_iso(created_ms);
            out.push(("text", ExtractedContent {
                target,
                text: data["text"].as_str().unwrap_or("").to_string(),
                tool_name: None,
                timestamp: ts.clone(),
                session_id: session_id.to_string(),
                edit_diff: None,
                raw_entry: finalize("text", &ts),
            }));
        }
        "reasoning" => {
            if targets.contains(&Target::Thinking) {
                let ts = millis_to_iso(time_start(data, created_ms));
                out.push(("reasoning", ExtractedContent {
                    target: Target::Thinking,
                    text: data["text"].as_str().unwrap_or("").to_string(),
                    tool_name: None,
                    timestamp: ts.clone(),
                    session_id: session_id.to_string(),
                    edit_diff: None,
                    raw_entry: finalize("reasoning", &ts),
                }));
            }
        }
        "tool" => {
            let tool = data["tool"].as_str().unwrap_or("").to_string();
            let state = &data["state"];
            let input = &state["input"];
            let output_obj = &state["output"];
            let is_bash = tool == "bash";

            // The call. For bash, emit BashCommand (the command text) when
            // wanted; always emit ToolUse (full input rendering) when wanted.
            // Distinct slots so both can coexist with the output.
            if is_bash && targets.contains(&Target::BashCommand) {
                let ts = millis_to_iso(time_input_start(state, created_ms));
                out.push(("cmd", ExtractedContent {
                    target: Target::BashCommand,
                    text: input["command"].as_str().unwrap_or("").to_string(),
                    tool_name: Some(tool.clone()),
                    timestamp: ts.clone(),
                    session_id: session_id.to_string(),
                    edit_diff: None,
                    raw_entry: finalize("cmd", &ts),
                }));
            }
            if targets.contains(&Target::ToolUse) && !tool.is_empty() {
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
                let ts = millis_to_iso(time_input_start(state, created_ms));
                out.push(("use", ExtractedContent {
                    target: Target::ToolUse,
                    text: format_tool_input(input),
                    tool_name: Some(tool.clone()),
                    timestamp: ts.clone(),
                    session_id: session_id.to_string(),
                    edit_diff,
                    raw_entry: finalize("use", &ts),
                }));
            }
            // The result, as its own target (BashOutput/ToolResult), when present.
            let out_target = if is_bash { Target::BashOutput } else { Target::ToolResult };
            if targets.contains(&out_target) && output_obj.as_str().map(|s| !s.is_empty()).unwrap_or(false) {
                let ts = millis_to_iso(time_output_end(state, updated_ms));
                out.push(("out", ExtractedContent {
                    target: out_target,
                    text: output_obj.as_str().unwrap_or("").to_string(),
                    tool_name: Some(tool.clone()),
                    timestamp: ts.clone(),
                    session_id: session_id.to_string(),
                    edit_diff: None,
                    raw_entry: finalize("out", &ts),
                }));
            }
        }
        "compaction" => {
            if targets.contains(&Target::CompactSummary) {
                let tail = data["tail_start_id"].as_str().unwrap_or("");
                let text = if tail.is_empty() {
                    "(compaction boundary)".to_string()
                } else {
                    format!("(compaction boundary; resumes at {})", tail)
                };
                let ts = millis_to_iso(created_ms);
                out.push(("compaction", ExtractedContent {
                    target: Target::CompactSummary,
                    text,
                    tool_name: None,
                    timestamp: ts.clone(),
                    session_id: session_id.to_string(),
                    edit_diff: None,
                    raw_entry: finalize("compaction", &ts),
                }));
            }
        }
        // step-start / step-finish / patch are step-boundary / snapshot markers
        // with no searchable prose; skip them.
        _ => {}
    }
    out
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    /// Build an opencode-shaped SQLite DB in a temp file and hand back a source
    /// plus the live connection (so tests can INSERT mid-scenario for follow).
    /// Minimal schema: only the columns the queries touch.
    struct Harness {
        src: OpenCodeSource,
        conn: Connection,
    }

    fn harness() -> Harness {
        let tmp = tempfile::NamedTempFile::new().unwrap();
        let path = tmp.into_temp_path().keep().unwrap();
        let conn = Connection::open(&path).unwrap();
        conn.execute_batch(
            "CREATE TABLE session (
                id TEXT PRIMARY KEY, project_id TEXT NOT NULL, parent_id TEXT,
                directory TEXT NOT NULL, time_created INTEGER NOT NULL,
                time_updated INTEGER NOT NULL);
             CREATE TABLE message (
                id TEXT PRIMARY KEY, session_id TEXT NOT NULL,
                time_created INTEGER NOT NULL, time_updated INTEGER NOT NULL,
                data TEXT NOT NULL);
             CREATE TABLE part (
                id TEXT PRIMARY KEY, message_id TEXT NOT NULL,
                session_id TEXT NOT NULL, time_created INTEGER NOT NULL,
                time_updated INTEGER NOT NULL, data TEXT NOT NULL);
             CREATE INDEX part_session_idx ON part(session_id);",
        ).unwrap();
        Harness { src: OpenCodeSource::new(path), conn }
    }

    fn add_session(h: &Harness, id: &str, dir: &str, is_sub: bool, ts: i64) {
        h.conn.execute(
            "INSERT INTO session(id, project_id, parent_id, directory, time_created, time_updated) \
             VALUES (?1,'proj',?2,?3,?4,?4)",
            rusqlite::params![id, if is_sub { Some("ses_parent") } else { None }, dir, ts],
        ).unwrap();
    }

    fn add_message(h: &Harness, id: &str, sid: &str, role: &str, ts: i64) {
        let data = serde_json::json!({ "role": role }).to_string();
        h.conn.execute(
            "INSERT INTO message(id, session_id, time_created, time_updated, data) \
             VALUES (?1,?2,?3,?3,?4)",
            rusqlite::params![id, sid, ts, data],
        ).unwrap();
    }

    /// Insert a part; `data` is the part.data JSON. Returns the new rowid.
    fn add_part(h: &Harness, id: &str, sid: &str, msg: &str, data: serde_json::Value, created: i64, updated: i64) -> i64 {
        h.conn.execute(
            "INSERT INTO part(id, message_id, session_id, time_created, time_updated, data) \
             VALUES (?1,?2,?3,?4,?5,?6)",
            rusqlite::params![id, msg, sid, created, updated, data.to_string()],
        ).unwrap();
        h.conn.query_row("SELECT rowid FROM part WHERE id=?1", [id], |r| r.get::<_, i64>(0)).unwrap()
    }

    fn all_targets() -> HashSet<Target> {
        [
            Target::User, Target::Assistant, Target::Thinking, Target::BashCommand,
            Target::BashOutput, Target::ToolUse, Target::ToolResult,
            Target::SubagentPrompt, Target::CompactSummary,
        ].into_iter().collect()
    }

    #[test]
    fn discover_sessions_and_projects() {
        let h = harness();
        add_session(&h, "ses_a", "/proj", false, 2000);
        add_session(&h, "ses_b", "/proj", true, 1500);
        add_session(&h, "ses_c", "/other", false, 3000);

        let s = h.src.discover_sessions("/proj");
        // Both top-level + subagent returned; newest first.
        let ids: Vec<&str> = s.iter().map(|x| x.session_id.as_str()).collect();
        assert_eq!(ids, vec!["ses_a", "ses_b"]);
        assert!(s.iter().find(|x| x.session_id == "ses_b").unwrap().is_subagent);
        assert_eq!(s.iter().find(|x| x.session_id == "ses_a").unwrap().backend, OPENCODE);

        let p = h.src.discover_projects();
        assert_eq!(p.len(), 2);
        assert!(p.iter().any(|x| x.decoded_path == "/proj" && x.session_count == 2));
    }

    #[test]
    fn extract_core_targets_and_roles() {
        let h = harness();
        add_session(&h, "ses_a", "/proj", false, 1000);
        add_message(&h, "msg_u", "ses_a", "user", 1100);
        add_message(&h, "msg_as", "ses_a", "assistant", 1200);
        add_part(&h, "p1", "ses_a", "msg_u", serde_json::json!({"type":"text","text":"hello user"}), 1100, 1100);
        add_part(&h, "p2", "ses_a", "msg_as", serde_json::json!({"type":"text","text":"hi back"}), 1200, 1200);
        add_part(&h, "p3", "ses_a", "msg_as", serde_json::json!({"type":"reasoning","text":"pondering"}), 1250, 1250);

        let sess = h.src.discover_sessions("/proj").into_iter().next().unwrap();
        let c = h.src.extract_content(&sess, &all_targets(), false);
        let targets: Vec<&Target> = c.iter().map(|x| &x.target).collect();
        assert!(targets.contains(&&Target::User), "missing user: {:?}", targets);
        assert!(targets.contains(&&Target::Assistant), "missing assistant");
        assert!(targets.contains(&&Target::Thinking), "missing thinking");
        assert_eq!(c.iter().find(|x| x.target == Target::User).unwrap().text, "hello user");
    }

    #[test]
    fn extract_bash_splits_command_and_output() {
        let h = harness();
        add_session(&h, "ses_a", "/proj", false, 1000);
        add_message(&h, "msg_as", "ses_a", "assistant", 1200);
        let tool = serde_json::json!({
            "type":"tool","tool":"bash","callID":"c1",
            "state":{"status":"completed",
                     "input":{"command":"ls -la","description":"list"},
                     "output":"file1\nfile2",
                     "time":{"start":1200,"end":1210}}
        });
        add_part(&h, "p1", "ses_a", "msg_as", tool, 1200, 1210);

        let sess = h.src.discover_sessions("/proj").into_iter().next().unwrap();
        let c = h.src.extract_content(&sess, &all_targets(), false);
        let cmd = c.iter().find(|x| x.target == Target::BashCommand).expect("bash-command record");
        assert_eq!(cmd.text, "ls -la");
        let out = c.iter().find(|x| x.target == Target::BashOutput).expect("bash-output record");
        assert_eq!(out.text, "file1\nfile2");
        // input timestamp precedes output timestamp
        assert!(cmd.timestamp <= out.timestamp, "input ts {} not <= output ts {}", cmd.timestamp, out.timestamp);
    }

    #[test]
    fn extract_edit_populates_diff() {
        let h = harness();
        add_session(&h, "ses_a", "/proj", false, 1000);
        add_message(&h, "msg_as", "ses_a", "assistant", 1200);
        let edit = serde_json::json!({
            "type":"tool","tool":"edit","callID":"c1",
            "state":{"status":"completed",
                     "input":{"filePath":"/x.rs","oldString":"fn old(){}","newString":"fn new(){}"},
                     "output":"done"}
        });
        add_part(&h, "p1", "ses_a", "msg_as", edit, 1200, 1210);

        let sess = h.src.discover_sessions("/proj").into_iter().next().unwrap();
        let c = h.src.extract_content(&sess, &all_targets(), false);
        let tu = c.iter().find(|x| x.target == Target::ToolUse && x.tool_name.as_deref() == Some("edit"))
            .expect("tool-use.edit record");
        let diff = tu.edit_diff.as_ref().expect("edit_diff populated");
        assert_eq!(diff.file_path, "/x.rs");
        assert_eq!(diff.old_string, "fn old(){}");
        assert_eq!(diff.new_string, "fn new(){}");
    }

    #[test]
    fn subagent_user_text_maps_to_subagent_prompt() {
        let h = harness();
        add_session(&h, "ses_parent", "/proj", false, 1000);
        add_session(&h, "ses_sub", "/proj", true, 1100);
        add_message(&h, "msg_u", "ses_sub", "user", 1200);
        add_part(&h, "p1", "ses_sub", "msg_u", serde_json::json!({"type":"text","text":"do the thing"}), 1200, 1200);

        let sub = h.src.discover_sessions("/proj").into_iter()
            .find(|s| s.session_id == "ses_sub").unwrap();
        assert!(sub.is_subagent);
        let c = h.src.extract_content(&sub, &all_targets(), false);
        let sp = c.iter().find(|x| x.target == Target::SubagentPrompt).expect("subagent-prompt");
        assert_eq!(sp.text, "do the thing");
        // A subagent's user text must NOT also surface as User.
        assert!(c.iter().all(|x| x.target != Target::User), "subagent text leaked into User target");
    }

    #[test]
    fn compaction_surfaces_as_compact_summary() {
        let h = harness();
        add_session(&h, "ses_a", "/proj", false, 1000);
        add_message(&h, "msg_u", "ses_a", "user", 1200);
        add_part(&h, "p1", "ses_a", "msg_u",
                 serde_json::json!({"type":"compaction","auto":false,"tail_start_id":"msg_xyz"}), 1200, 1200);

        let sess = h.src.discover_sessions("/proj").into_iter().next().unwrap();
        let c = h.src.extract_content(&sess, &all_targets(), false);
        assert!(c.iter().any(|x| x.target == Target::CompactSummary), "compaction not surfaced");
    }

    #[test]
    fn json_envelope_is_cross_backend_portable() {
        // `--json` raw entries must carry the cross-backend envelope keys
        // (sessionId, timestamp, type=role, partType, slot) so `jq .sessionId`
        // and `select(.type=="user")` port to Claude.
        let h = harness();
        add_session(&h, "ses_a", "/proj", false, 1000);
        add_message(&h, "msg_u", "ses_a", "user", 1100);
        add_message(&h, "msg_as", "ses_a", "assistant", 1200);
        add_part(&h, "p1", "ses_a", "msg_u", serde_json::json!({"type":"text","text":"hi"}), 1100, 1100);
        add_part(&h, "p2", "ses_a", "msg_as",
                 serde_json::json!({"type":"tool","tool":"bash","state":{"status":"completed","input":{"command":"ls"},"output":"f1"}}),
                 1200, 1210);

        let sess = h.src.discover_sessions("/proj").into_iter().next().unwrap();
        let c = h.src.extract_content(&sess, &all_targets(), true);

        let user_rec = c.iter().find(|x| x.target == Target::User).unwrap();
        let raw = user_rec.raw_entry.as_ref().unwrap();
        assert_eq!(raw["sessionId"], "ses_a");
        assert_eq!(raw["type"], "user", "type normalized to role");
        assert_eq!(raw["partType"], "text", "native part type preserved");
        assert_eq!(raw["role"], "user");
        assert!(raw["timestamp"].is_string());
        assert_eq!(raw["slot"], "text");
        assert_eq!(raw["text"], "hi", "native field preserved at top level");

        // Tool part yields cmd/use/out slots; each envelope carries its own slot+ts.
        let cmd = c.iter().find(|x| x.target == Target::BashCommand).unwrap();
        let out = c.iter().find(|x| x.target == Target::BashOutput).unwrap();
        assert_eq!(cmd.raw_entry.as_ref().unwrap()["slot"], "cmd");
        assert_eq!(out.raw_entry.as_ref().unwrap()["slot"], "out");
        assert_eq!(cmd.raw_entry.as_ref().unwrap()["type"], "assistant");
        assert_eq!(out.raw_entry.as_ref().unwrap()["partType"], "tool");
        // input (cmd) precedes output (out) in time.
        assert!(cmd.timestamp <= out.timestamp);
    }

    #[test]
    fn follow_streams_new_part_once() {
        let h = harness();
        add_session(&h, "ses_a", "/proj", false, 1000);
        add_message(&h, "msg_as", "ses_a", "assistant", 1200);
        add_part(&h, "p1", "ses_a", "msg_as", serde_json::json!({"type":"text","text":"old"}), 1200, 1200);

        let _sess = h.src.discover_sessions("/proj").into_iter().next().unwrap();
        let conn = h.src.open().unwrap();
        let mut stmt = conn.prepare(
            "SELECT p.rowid, p.data, p.time_created, p.time_updated, json_extract(m.data,'$.role') \
             FROM part p LEFT JOIN message m ON m.id=p.message_id \
             WHERE p.session_id=?1 AND (p.rowid>?2 OR p.time_updated>?3) ORDER BY p.rowid",
        ).unwrap();
        // Seed at current end (caller already tailed the initial records).
        let (mut last_rowid, mut last_updated) = conn.query_row(
            "SELECT COALESCE(MAX(rowid),0), COALESCE(MAX(time_updated),0) FROM part WHERE session_id='ses_a'",
            [], |r| Ok((r.get::<_,i64>(0)?, r.get::<_,i64>(1)?)),
        ).unwrap();
        let mut emitted: std::collections::HashSet<(i64, &'static str)> = Default::default();
        let mut targets = HashSet::new();
        targets.insert(Target::Assistant);

        // First poll after seeding: nothing new.
        let batch = poll_follow_once(&mut stmt, "ses_a", false, &targets, &mut last_rowid, &mut last_updated, &mut emitted);
        assert!(batch.is_empty(), "seeded follow should not replay history");

        // Insert a NEW part (the live agent producing output).
        add_part(&h, "p2", "ses_a", "msg_as", serde_json::json!({"type":"text","text":"fresh line"}), 1300, 1300);

        // Second poll: the new record streams exactly once.
        let batch = poll_follow_once(&mut stmt, "ses_a", false, &targets, &mut last_rowid, &mut last_updated, &mut emitted);
        assert_eq!(batch.len(), 1);
        assert_eq!(batch[0].text, "fresh line");
        // Third poll: not re-emitted (idempotent).
        let batch = poll_follow_once(&mut stmt, "ses_a", false, &targets, &mut last_rowid, &mut last_updated, &mut emitted);
        assert!(batch.is_empty(), "already-emitted slot must not repeat");
    }

    #[test]
    fn follow_streams_tool_output_when_it_fills_in() {
        // A tool call starts (no output yet) then completes (output fills in).
        // The command slot streams on arrival; the output slot streams later,
        // once the part's time_updated advances — without re-emitting the command.
        let h = harness();
        add_session(&h, "ses_a", "/proj", false, 1000);
        add_message(&h, "msg_as", "ses_a", "assistant", 1200);

        let conn = h.src.open().unwrap();
        let mut stmt = conn.prepare(
            "SELECT p.rowid, p.data, p.time_created, p.time_updated, json_extract(m.data,'$.role') \
             FROM part p LEFT JOIN message m ON m.id=p.message_id \
             WHERE p.session_id=?1 AND (p.rowid>?2 OR p.time_updated>?3) ORDER BY p.rowid",
        ).unwrap();
        // Seed at the (empty) end of the session, as a real follow would.
        let (mut last_rowid, mut last_updated) = (0i64, 0i64);
        let mut emitted: std::collections::HashSet<(i64, &'static str)> = Default::default();
        let mut targets = HashSet::new();
        targets.insert(Target::BashCommand);
        targets.insert(Target::BashOutput);

        // Tool call starts: command present, output empty.
        add_part(&h, "p1", "ses_a", "msg_as",
            serde_json::json!({"type":"tool","tool":"bash","state":{"status":"running","input":{"command":"ls"},"output":""}}),
            1200, 1200);

        // First poll: only the command slot streams (output empty → no out slot).
        let batch = poll_follow_once(&mut stmt, "ses_a", false, &targets, &mut last_rowid, &mut last_updated, &mut emitted);
        assert!(batch.iter().any(|r| r.target == Target::BashCommand), "command should stream");
        assert!(!batch.iter().any(|r| r.target == Target::BashOutput), "no output yet");

        // The tool completes: output fills in, time_updated advances.
        let new_output = serde_json::json!({"type":"tool","tool":"bash","state":{"status":"completed","input":{"command":"ls"},"output":"file1"}});
        h.conn.execute("UPDATE part SET data=?1, time_updated=1250 WHERE id='p1'",
                       rusqlite::params![new_output.to_string()]).unwrap();

        // Second poll: the output slot streams; the command slot does NOT repeat.
        let batch = poll_follow_once(&mut stmt, "ses_a", false, &targets, &mut last_rowid, &mut last_updated, &mut emitted);
        assert!(batch.iter().any(|r| r.target == Target::BashOutput), "output should stream once filled");
        assert!(!batch.iter().any(|r| r.target == Target::BashCommand), "command must not re-emit");
    }
}
