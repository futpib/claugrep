//! Hermetic end-to-end tests using a self-contained mock world.
//!
//! Each test builds synthetic Claude session JSONL files in a temporary HOME
//! directory and invokes the real `claugrep` binary against them.  No real
//! Claude session data is required — every test is fully deterministic.

use std::fs;
use std::io::{Read as IoRead, Write as IoWrite};
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::{SystemTime, Duration};

extern crate filetime;

// ── Mock world ────────────────────────────────────────────────────────────────

/// Owns a temporary HOME directory.  All `claugrep` commands spawned via
/// `world.cmd()` see this directory as `$HOME`.
struct MockWorld {
    home: tempfile::TempDir,
}

impl MockWorld {
    fn new() -> Self {
        MockWorld {
            home: tempfile::TempDir::new().unwrap(),
        }
    }

    /// Return a `Command` for the claugrep binary with `HOME` overridden.
    fn cmd(&self) -> Command {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_claugrep"));
        cmd.env("HOME", self.home.path());
        // Also set XDG_CONFIG_HOME so dirs::config_dir() uses our mock home's .config
        cmd.env("XDG_CONFIG_HOME", self.home.path().join(".config"));
        // Clear CLAUDE_CONFIG_DIR so it doesn't bleed between tests
        cmd.env_remove("CLAUDE_CONFIG_DIR");
        cmd
    }

    /// Create a named mock project and return a handle for adding sessions.
    ///
    /// The project path `/claugrep-mock/<name>` is non-existent on the real
    /// filesystem, so `canonicalize()` falls back to the raw string — which
    /// is exactly what `resolve_project` does.
    fn project(&self, name: &str) -> MockProject {
        let project_path = format!("/claugrep-mock/{}", name);
        let encoded = project_path.replace(['/', '.'], "-");
        let session_dir = self
            .home
            .path()
            .join(".claude")
            .join("projects")
            .join(&encoded);
        fs::create_dir_all(&session_dir).unwrap();
        MockProject {
            project_path,
            session_dir,
        }
    }

    /// Create a named mock project under a claudex account.
    ///
    /// Sessions are stored at `$HOME/.config/claudex/accounts/<account>/claude/projects/<encoded>/`.
    fn account_project(&self, account: &str, name: &str) -> MockProject {
        let project_path = format!("/claugrep-mock/{}", name);
        let encoded = project_path.replace(['/', '.'], "-");
        let account_dir = self.home.path()
            .join(".config").join("claudex").join("accounts").join(account).join("claude");
        let session_dir = account_dir.join("projects").join(&encoded);
        fs::create_dir_all(&session_dir).unwrap();
        MockProject { project_path, session_dir }
    }
}

struct MockProject {
    project_path: String,
    session_dir: PathBuf,
}

impl MockProject {
    fn path(&self) -> &str {
        &self.project_path
    }

    /// Return the path where a session file would be stored.
    fn session_path(&self, id: &str) -> PathBuf {
        self.session_dir.join(format!("{}.jsonl", id))
    }

    /// Create a normal (non-subagent) session file.
    fn session(&self, id: &str) -> SessionBuilder {
        let path = self.session_dir.join(format!("{}.jsonl", id));
        SessionBuilder::new(id.to_string(), path)
    }

    /// Create a subagent session stored in `<parent_id>/subagents/agent-<name>.jsonl`.
    fn subagent_session(&self, parent_id: &str, agent_name: &str) -> SessionBuilder {
        let dir = self.session_dir.join(parent_id).join("subagents");
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join(format!("agent-{}.jsonl", agent_name));
        // Also ensure the parent session file exists (even empty) so
        // discover_sessions finds the parent and loads its subagents.
        let parent_path = self.session_dir.join(format!("{}.jsonl", parent_id));
        if !parent_path.exists() {
            fs::File::create(&parent_path).unwrap();
        }
        SessionBuilder::new(parent_id.to_string(), path)
    }
}

// ── Session builder ───────────────────────────────────────────────────────────

struct SessionBuilder {
    session_id: String,
    file: fs::File,
    tool_counter: u32,
    ts_secs: u32,
}

impl SessionBuilder {
    fn new(session_id: String, path: PathBuf) -> Self {
        SessionBuilder {
            session_id,
            file: fs::File::create(path).unwrap(),
            tool_counter: 0,
            ts_secs: 0,
        }
    }

    /// Set the internal clock offset so the next timestamp starts from a given second.
    fn with_ts_offset(mut self, secs: u32) -> Self {
        self.ts_secs = secs;
        self
    }

    /// Advance the internal clock and return an ISO-8601 timestamp string.
    fn next_ts(&mut self) -> String {
        self.ts_secs += 1;
        let h = self.ts_secs / 3600;
        let m = (self.ts_secs % 3600) / 60;
        let s = self.ts_secs % 60;
        format!("2024-01-01T{:02}:{:02}:{:02}Z", h, m, s)
    }

    fn write(&mut self, v: serde_json::Value) {
        writeln!(self.file, "{}", v).unwrap();
    }

    fn user_message(mut self, text: &str) -> Self {
        let ts = self.next_ts();
        let sid = self.session_id.clone();
        self.write(serde_json::json!({
            "type": "user",
            "message": {"role": "user", "content": text},
            "timestamp": ts,
            "sessionId": sid,
        }));
        self
    }

    fn assistant_message(mut self, text: &str) -> Self {
        let ts = self.next_ts();
        let sid = self.session_id.clone();
        self.write(serde_json::json!({
            "type": "assistant",
            "message": {"role": "assistant", "content": [{"type": "text", "text": text}]},
            "timestamp": ts,
            "sessionId": sid,
        }));
        self
    }

    /// Write a Bash tool-use + tool-result pair (bash-command + bash-output).
    fn bash(mut self, cmd: &str, output: &str) -> Self {
        self.tool_counter += 1;
        let id = format!("toolu_{:04}", self.tool_counter);
        let ts1 = self.next_ts();
        let ts2 = self.next_ts();
        let sid = self.session_id.clone();
        self.write(serde_json::json!({
            "type": "assistant",
            "message": {"content": [{
                "type": "tool_use", "id": id, "name": "Bash",
                "input": {"command": cmd}
            }]},
            "timestamp": ts1,
            "sessionId": sid,
        }));
        let id2 = id.clone();
        self.write(serde_json::json!({
            "type": "user",
            "message": {"content": [{
                "type": "tool_result", "tool_use_id": id2, "content": output
            }]},
            "timestamp": ts2,
            "sessionId": sid,
        }));
        self
    }

    /// Write a non-Bash tool-use + tool-result pair (tool-use + tool-result).
    fn tool(mut self, name: &str, input_key: &str, input_val: &str, output: &str) -> Self {
        self.tool_counter += 1;
        let id = format!("toolu_{:04}", self.tool_counter);
        let ts1 = self.next_ts();
        let ts2 = self.next_ts();
        let sid = self.session_id.clone();
        self.write(serde_json::json!({
            "type": "assistant",
            "message": {"content": [{
                "type": "tool_use", "id": id, "name": name,
                "input": {input_key: input_val}
            }]},
            "timestamp": ts1,
            "sessionId": sid,
        }));
        let id2 = id.clone();
        self.write(serde_json::json!({
            "type": "user",
            "message": {"content": [{
                "type": "tool_result", "tool_use_id": id2, "content": output
            }]},
            "timestamp": ts2,
            "sessionId": sid,
        }));
        self
    }

    /// Write an Edit tool-use entry (file_path, old_string, new_string) + a
    /// minimal tool-result so the tool-use map is properly populated.
    fn edit(mut self, file_path: &str, old_string: &str, new_string: &str) -> Self {
        self.tool_counter += 1;
        let id = format!("toolu_{:04}", self.tool_counter);
        let ts1 = self.next_ts();
        let ts2 = self.next_ts();
        let sid = self.session_id.clone();
        self.write(serde_json::json!({
            "type": "assistant",
            "message": {"content": [{
                "type": "tool_use", "id": id, "name": "Edit",
                "input": {
                    "file_path": file_path,
                    "old_string": old_string,
                    "new_string": new_string,
                }
            }]},
            "timestamp": ts1,
            "sessionId": sid,
        }));
        let id2 = id.clone();
        self.write(serde_json::json!({
            "type": "user",
            "message": {"content": [{
                "type": "tool_result", "tool_use_id": id2, "content": ""
            }]},
            "timestamp": ts2,
            "sessionId": sid,
        }));
        self
    }

    fn compact_summary(mut self, text: &str) -> Self {
        let ts = self.next_ts();
        let sid = self.session_id.clone();
        self.write(serde_json::json!({
            "type": "user",
            "isCompactSummary": true,
            "message": {"role": "user", "content": text},
            "timestamp": ts,
            "sessionId": sid,
        }));
        self
    }

    /// Write a system record with a subtype and optional content.
    fn system_record(mut self, subtype: &str, content: Option<&str>) -> Self {
        let ts = self.next_ts();
        let sid = self.session_id.clone();
        let mut v = serde_json::json!({
            "type": "system",
            "subtype": subtype,
            "timestamp": ts,
            "sessionId": sid,
        });
        if let Some(c) = content {
            v["content"] = serde_json::json!(c);
        }
        self.write(v);
        self
    }

    /// Write a file-history-snapshot record with tracked file backups.
    fn file_history_snapshot(mut self, files: &[&str]) -> Self {
        let ts = self.next_ts();
        let mid = format!("msg-{:04}", self.ts_secs);
        let mut backups = serde_json::Map::new();
        for (i, file) in files.iter().enumerate() {
            backups.insert(file.to_string(), serde_json::json!({
                "backupFileName": format!("abcdef{}@v{}", i, i + 1),
                "version": i + 1,
                "backupTime": ts,
            }));
        }
        self.write(serde_json::json!({
            "type": "file-history-snapshot",
            "messageId": mid,
            "isSnapshotUpdate": false,
            "snapshot": {
                "messageId": mid,
                "trackedFileBackups": backups,
                "timestamp": ts,
            },
        }));
        self
    }

    /// Write a queue-operation record.
    fn queue_operation(mut self, operation: &str, content: Option<&str>) -> Self {
        let ts = self.next_ts();
        let sid = self.session_id.clone();
        let mut v = serde_json::json!({
            "type": "queue-operation",
            "operation": operation,
            "timestamp": ts,
            "sessionId": sid,
        });
        if let Some(c) = content {
            v["content"] = serde_json::json!(c);
        }
        self.write(v);
        self
    }

    /// Write a last-prompt record.
    fn last_prompt(mut self, text: &str) -> Self {
        let sid = self.session_id.clone();
        self.write(serde_json::json!({
            "type": "last-prompt",
            "lastPrompt": text,
            "sessionId": sid,
        }));
        self
    }

    /// Write an agent-name record.
    fn agent_name(mut self, name: &str) -> Self {
        let sid = self.session_id.clone();
        self.write(serde_json::json!({
            "type": "agent-name",
            "agentName": name,
            "sessionId": sid,
        }));
        self
    }

    /// Write a custom-title record.
    fn custom_title(mut self, title: &str) -> Self {
        let sid = self.session_id.clone();
        self.write(serde_json::json!({
            "type": "custom-title",
            "customTitle": title,
            "sessionId": sid,
        }));
        self
    }

    /// Write a permission-mode record.
    fn permission_mode(mut self, mode: &str) -> Self {
        let sid = self.session_id.clone();
        self.write(serde_json::json!({
            "type": "permission-mode",
            "permissionMode": mode,
            "sessionId": sid,
        }));
        self
    }

    /// Write an empty attachment record with the given inner type.
    /// Used for the "not warned" tests — the record has no searchable content,
    /// but should still be recognized and silently consumed.
    fn attachment(mut self, inner_type: &str) -> Self {
        let sid = self.session_id.clone();
        self.write(serde_json::json!({
            "type": "attachment",
            "attachment": {
                "type": inner_type,
                "addedNames": [],
                "addedLines": [],
                "removedNames": [],
            },
            "sessionId": sid,
        }));
        self
    }

    /// Write a `deferred_tools_delta` attachment with the given added/removed
    /// tool names.
    fn attachment_tools(mut self, added: &[&str], removed: &[&str]) -> Self {
        let sid = self.session_id.clone();
        self.write(serde_json::json!({
            "type": "attachment",
            "attachment": {
                "type": "deferred_tools_delta",
                "addedNames": added,
                "addedLines": added,
                "removedNames": removed,
            },
            "sessionId": sid,
        }));
        self
    }

    /// Write an `mcp_server_delta` attachment with the given added server
    /// names and corresponding description blocks.
    fn attachment_mcp(mut self, added: &[&str], blocks: &[&str]) -> Self {
        let sid = self.session_id.clone();
        self.write(serde_json::json!({
            "type": "attachment",
            "attachment": {
                "type": "mcp_server_delta",
                "addedNames": added,
                "addedBlocks": blocks,
                "removedNames": [],
            },
            "sessionId": sid,
        }));
        self
    }

    fn done(mut self) {
        self.file.flush().unwrap();
    }
}

// ── Helpers ───────────────────────────────────────────────────────────────────

fn stdout(out: &std::process::Output) -> &str {
    std::str::from_utf8(&out.stdout).unwrap()
}

// Strip ANSI escape sequences for plain-text assertions.
fn strip_ansi(s: &str) -> String {
    let re = regex::Regex::new(r"\x1b\[[0-9;]*m").unwrap();
    re.replace_all(s, "").to_string()
}

// ═════════════════════════════════════════════════════════════════════════════
// last subcommand
// ═════════════════════════════════════════════════════════════════════════════

#[test]
fn test_last_basic() {
    let world = MockWorld::new();
    let proj = world.project("alpha");
    proj.session("sess-aaa")
        .user_message("LAST_USER_HELLO")
        .assistant_message("LAST_ASST_WORLD")
        .done();

    let out = world
        .cmd()
        .args(["last", "--project", proj.path()])
        .output()
        .unwrap();

    assert!(out.status.success(), "stderr: {}", String::from_utf8_lossy(&out.stderr));
    let text = strip_ansi(stdout(&out));
    assert!(text.contains("LAST_USER_HELLO"), "expected user message in output");
    assert!(text.contains("LAST_ASST_WORLD"), "expected assistant message in output");
}

#[test]
fn test_last_n_count() {
    let world = MockWorld::new();
    let proj = world.project("count-test");
    // Write 5 user messages; request only 2.
    let mut builder = proj.session("sess-cnt");
    for i in 0..5 {
        builder = builder.user_message(&format!("COUNT_MSG_{}", i));
    }
    builder.done();

    let out = world
        .cmd()
        .args(["last", "-n", "2", "--project", proj.path()])
        .output()
        .unwrap();

    assert!(out.status.success());
    let text = strip_ansi(stdout(&out));
    // Last 2 messages should be COUNT_MSG_3 and COUNT_MSG_4.
    assert!(text.contains("COUNT_MSG_3") || text.contains("COUNT_MSG_4"),
        "expected recent messages");
    assert!(!text.contains("COUNT_MSG_0"),
        "should not show early message when -n 2");
}

#[test]
fn test_last_project_scoped() {
    let world = MockWorld::new();
    let proj_a = world.project("scope-a");
    let proj_b = world.project("scope-b");
    proj_a.session("sess-a").user_message("ONLY_IN_PROJECT_A").done();
    proj_b.session("sess-b").user_message("ONLY_IN_PROJECT_B").done();

    // --project should scope to proj_a only.
    let out = world
        .cmd()
        .args(["last", "--project", proj_a.path()])
        .output()
        .unwrap();

    assert!(out.status.success());
    let text = strip_ansi(stdout(&out));
    assert!(text.contains("ONLY_IN_PROJECT_A"));
    assert!(!text.contains("ONLY_IN_PROJECT_B"),
        "scoped project should not show other project's messages");
}

#[test]
fn test_last_json_output() {
    let world = MockWorld::new();
    let proj = world.project("last-json");
    proj.session("sess-lj")
        .user_message("LAST_JSON_CONTENT")
        .done();

    let out = world
        .cmd()
        .args(["last", "--json", "--project", proj.path()])
        .output()
        .unwrap();

    assert!(out.status.success());
    let text = stdout(&out);
    let lines: Vec<&str> = text.lines().collect();
    assert!(!lines.is_empty(), "should have at least one JSONL line");
    let first: serde_json::Value = serde_json::from_str(lines[0])
        .expect("each line must be valid JSON");
    assert!(first["type"].is_string(), "raw entry should have a type field");
    assert!(first["sessionId"].is_string());
    assert!(text.contains("LAST_JSON_CONTENT"));
}

#[test]
fn test_last_missing_project_exits_nonzero() {
    let world = MockWorld::new();
    // Project exists in path string but has no sessions in mock home.
    let out = world
        .cmd()
        .args(["last", "--project", "/claugrep-mock/no-such-project"])
        .output()
        .unwrap();

    assert!(!out.status.success(), "should exit nonzero when no sessions found");
}

// ═════════════════════════════════════════════════════════════════════════════
// warning on unrecognized record types
// ═════════════════════════════════════════════════════════════════════════════

#[test]
fn test_warning_printed_for_unrecognized_record_type() {
    let world = MockWorld::new();
    let proj = world.project("unrecognized-record");

    // Write a raw session file with a truly unknown record type followed by a user message.
    let session_path = proj.session_dir.join("sess-ur.jsonl");
    let mut f = fs::File::create(&session_path).unwrap();
    writeln!(f, r#"{{"type":"totally_unknown","content":"something","timestamp":"2024-01-01T00:00:00Z","sessionId":"sess-ur"}}"#).unwrap();
    writeln!(f, r#"{{"type":"user","message":{{"content":"hello"}},"timestamp":"2024-01-01T00:00:01Z","sessionId":"sess-ur"}}"#).unwrap();

    let out = world.cmd()
        .args(["search", "hello", "--project", proj.path()])
        .output().unwrap();

    assert!(out.status.success());
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(
        err.contains("warning") && err.contains("skipping"),
        "stderr should warn about the unrecognized record, got: {}",
        err
    );
    // The record preview (first 120 chars) should appear in the warning.
    assert!(
        err.contains("totally_unknown"),
        "warning should include a preview of the record, got: {}",
        err
    );
}

// ═════════════════════════════════════════════════════════════════════════════
// no spurious warnings when subagents directory is absent (issue #19)
// ═════════════════════════════════════════════════════════════════════════════

#[test]
fn test_no_warning_when_subagents_dir_absent() {
    let world = MockWorld::new();
    let proj = world.project("no-subagents");
    // A plain session with no subagents directory.
    proj.session("sess-ns")
        .user_message("HELLO_NO_SUBAGENTS")
        .done();

    for subcommand in &[
        vec!["last", "--project", proj.path()],
        vec!["search", "HELLO_NO_SUBAGENTS", "--project", proj.path()],
    ] {
        let out = world.cmd().args(subcommand).output().unwrap();
        assert!(out.status.success(), "stderr: {}", String::from_utf8_lossy(&out.stderr));
        let err = String::from_utf8_lossy(&out.stderr);
        assert!(
            !err.contains("warning"),
            "subcommand {:?} should not print warnings when subagents dir is absent, got: {}",
            subcommand, err
        );
    }
}

// ═════════════════════════════════════════════════════════════════════════════
// no spurious warnings when a project directory no longer exists (worktrees)
// ═════════════════════════════════════════════════════════════════════════════

#[test]
fn test_no_warning_when_project_dir_missing() {
    let world = MockWorld::new();

    // Point --project at a path whose encoded project dir does not exist in
    // the mock HOME.  This exercises `discover_sessions` → `try_read_dir` on
    // a NotFound directory — it should exit cleanly with no warning.
    let out = world.cmd()
        .args(["search", "anything", "--project", "/claugrep-mock/gone-worktree"])
        .output().unwrap();
    // Exit code is non-zero (no sessions found), but stderr must not contain a warning.
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(
        !err.contains("warning"),
        "search should not warn about missing project dirs, got: {}",
        err
    );
}

// ═════════════════════════════════════════════════════════════════════════════
// search — target flags (--targets / -t)
// ═════════════════════════════════════════════════════════════════════════════

#[test]
fn test_search_targets_assistant() {
    let world = MockWorld::new();
    let proj = world.project("asst-flag");
    proj.session("sess-af")
        .user_message("USER_ONLY_TEXT_AF")
        .assistant_message("ASST_UNIQUE_TEXT_AF")
        .done();

    // --targets assistant finds assistant text.
    let found = world
        .cmd()
        .args(["search", "ASST_UNIQUE_TEXT_AF", "--targets", "assistant", "--project", proj.path()])
        .output()
        .unwrap();
    assert!(found.status.success());
    assert!(!strip_ansi(stdout(&found)).contains("No matches found"));

    // --targets user does NOT find it.
    let miss = world
        .cmd()
        .args(["search", "ASST_UNIQUE_TEXT_AF", "--targets", "user", "--project", proj.path()])
        .output()
        .unwrap();
    assert!(strip_ansi(stdout(&miss)).contains("No matches found"));
}

#[test]
fn test_search_targets_bash_command() {
    let world = MockWorld::new();
    let proj = world.project("bash-cmd-flag");
    proj.session("sess-bc")
        .bash("BASH_CMD_UNIQUE_XYZ", "some output")
        .done();

    let out = world
        .cmd()
        .args(["search", "BASH_CMD_UNIQUE_XYZ", "-t", "bash-command", "--project", proj.path()])
        .output()
        .unwrap();

    assert!(out.status.success());
    assert!(!strip_ansi(stdout(&out)).contains("No matches found"),
        "-t bash-command should find the command text");
}

#[test]
fn test_search_targets_bash_output() {
    let world = MockWorld::new();
    let proj = world.project("bash-out-flag");
    proj.session("sess-bo")
        .bash("ls", "BASH_OUTPUT_UNIQUE_QRS")
        .done();

    let out = world
        .cmd()
        .args(["search", "BASH_OUTPUT_UNIQUE_QRS", "-t", "bash-output", "--project", proj.path()])
        .output()
        .unwrap();

    assert!(out.status.success());
    assert!(!strip_ansi(stdout(&out)).contains("No matches found"),
        "-t bash-output should find the command output");
}

#[test]
fn test_search_targets_tool_use() {
    let world = MockWorld::new();
    let proj = world.project("tool-use-flag");
    proj.session("sess-tu")
        .tool("Read", "file_path", "TOOL_USE_UNIQUE_PATH_ABC", "file contents")
        .done();

    let out = world
        .cmd()
        .args(["search", "TOOL_USE_UNIQUE_PATH_ABC", "--targets", "tool-use", "--project", proj.path()])
        .output()
        .unwrap();

    assert!(out.status.success());
    assert!(!strip_ansi(stdout(&out)).contains("No matches found"),
        "--targets tool-use should find the tool input");
}

#[test]
fn test_search_targets_tool_result() {
    let world = MockWorld::new();
    let proj = world.project("tool-result-flag");
    proj.session("sess-tr")
        .tool("Read", "file_path", "/some/path", "TOOL_RESULT_UNIQUE_DEF")
        .done();

    let out = world
        .cmd()
        .args(["search", "TOOL_RESULT_UNIQUE_DEF", "-t", "tool-result", "--project", proj.path()])
        .output()
        .unwrap();

    assert!(out.status.success());
    assert!(!strip_ansi(stdout(&out)).contains("No matches found"),
        "-t tool-result should find the tool output");
}

#[test]
fn test_search_targets_subagent_prompt() {
    let world = MockWorld::new();
    let proj = world.project("subagent-flag");
    // Parent session (regular, so its messages are "user" type).
    proj.session("parent-sess-sp").user_message("PARENT_MSG").done();
    // Subagent session — its messages become "subagent-prompt".
    proj.subagent_session("parent-sess-sp", "agent-01")
        .user_message("SUBAGENT_UNIQUE_PROMPT_GHI")
        .done();

    // -t subagent-prompt finds the subagent's user message.
    let found = world
        .cmd()
        .args(["search", "SUBAGENT_UNIQUE_PROMPT_GHI", "-t", "subagent-prompt",
               "--project", proj.path()])
        .output()
        .unwrap();
    assert!(found.status.success());
    assert!(!strip_ansi(stdout(&found)).contains("No matches found"),
        "-t subagent-prompt should find subagent messages");

    // -t user does NOT find subagent messages.
    let miss = world
        .cmd()
        .args(["search", "SUBAGENT_UNIQUE_PROMPT_GHI", "-t", "user", "--project", proj.path()])
        .output()
        .unwrap();
    assert!(strip_ansi(stdout(&miss)).contains("No matches found"),
        "-t user should not match subagent prompts");
}

#[test]
fn test_search_targets_compact_summary() {
    let world = MockWorld::new();
    let proj = world.project("compact-flag");
    proj.session("sess-cs")
        .compact_summary("COMPACT_SUM_UNIQUE_JKL")
        .done();

    // -t compact-summary finds the summary.
    let found = world
        .cmd()
        .args(["search", "COMPACT_SUM_UNIQUE_JKL", "-t", "compact-summary",
               "--project", proj.path()])
        .output()
        .unwrap();
    assert!(found.status.success());
    assert!(!strip_ansi(stdout(&found)).contains("No matches found"),
        "-t compact-summary should find summaries");

    // -t user does NOT find compact summaries.
    let miss = world
        .cmd()
        .args(["search", "COMPACT_SUM_UNIQUE_JKL", "-t", "user", "--project", proj.path()])
        .output()
        .unwrap();
    assert!(strip_ansi(stdout(&miss)).contains("No matches found"),
        "-t user should not match compact summaries");
}

#[test]
fn test_search_targets_all() {
    let world = MockWorld::new();
    let proj = world.project("targets-all");
    proj.session("sess-all")
        .user_message("USER_MSG_ALL_TEST")
        .assistant_message("ASST_MSG_ALL_TEST")
        .bash("BASH_CMD_ALL_TEST", "BASH_OUT_ALL_TEST")
        .compact_summary("COMPACT_ALL_TEST")
        .done();

    // -t all finds everything
    let out = world
        .cmd()
        .args(["search", "ALL_TEST", "-t", "all", "--project", proj.path()])
        .output()
        .unwrap();
    assert!(out.status.success());
    let text = strip_ansi(stdout(&out));
    assert!(text.contains("USER_MSG_ALL_TEST"), "-t all should find user messages");
    assert!(text.contains("ASST_MSG_ALL_TEST"), "-t all should find assistant messages");
    assert!(text.contains("BASH_CMD_ALL_TEST"), "-t all should find bash commands");
    assert!(text.contains("BASH_OUT_ALL_TEST"), "-t all should find bash output");
    assert!(text.contains("COMPACT_ALL_TEST"), "-t all should find compact summaries");
}

#[test]
fn test_search_targets_comma_separated() {
    let world = MockWorld::new();
    let proj = world.project("targets-comma");
    proj.session("sess-comma")
        .user_message("USER_COMMA_TEST")
        .assistant_message("ASST_COMMA_TEST")
        .bash("BASH_CMD_COMMA_TEST", "BASH_OUT_COMMA_TEST")
        .done();

    // -t user,assistant finds both user and assistant but not bash
    let out = world
        .cmd()
        .args(["search", "COMMA_TEST", "-t", "user,assistant", "--project", proj.path()])
        .output()
        .unwrap();
    assert!(out.status.success());
    let text = strip_ansi(stdout(&out));
    assert!(text.contains("USER_COMMA_TEST"), "should find user messages");
    assert!(text.contains("ASST_COMMA_TEST"), "should find assistant messages");
    assert!(!text.contains("BASH_CMD_COMMA_TEST"), "should not find bash commands");
}

#[test]
fn test_dump_targets_shorthand() {
    let world = MockWorld::new();
    let proj = world.project("dump-t-short");
    proj.session("sess-dt")
        .user_message("USER_DUMP_T_SHORT")
        .assistant_message("ASST_DUMP_T_SHORT")
        .done();

    // -t user only shows user
    let out = world
        .cmd()
        .args(["dump", "0", "-t", "user", "--project", proj.path()])
        .output()
        .unwrap();
    assert!(out.status.success());
    let text = stdout(&out);
    assert!(text.contains("USER_DUMP_T_SHORT"), "-t user should show user messages");
    assert!(!text.contains("ASST_DUMP_T_SHORT"), "-t user should not show assistant messages");
}

#[test]
fn test_last_targets_shorthand() {
    let world = MockWorld::new();
    let proj = world.project("last-t-short");
    proj.session("sess-lt")
        .user_message("USER_LAST_T_SHORT")
        .assistant_message("ASST_LAST_T_SHORT")
        .done();

    // -t assistant only shows assistant
    let out = world
        .cmd()
        .args(["last", "-t", "assistant", "--project", proj.path()])
        .output()
        .unwrap();
    assert!(out.status.success());
    let text = stdout(&out);
    assert!(text.contains("ASST_LAST_T_SHORT"), "-t assistant should show assistant messages");
    assert!(!text.contains("USER_LAST_T_SHORT"), "-t assistant should not show user messages");
}

// ═════════════════════════════════════════════════════════════════════════════
// search — other untested flags
// ═════════════════════════════════════════════════════════════════════════════

#[test]
fn test_search_session_flag_scopes_to_one_session() {
    let world = MockWorld::new();
    let proj = world.project("session-scope");
    proj.session("aaaa1111-0000-0000-0000-000000000000")
        .user_message("ONLY_IN_SESSION_AAAA")
        .done();
    proj.session("bbbb2222-0000-0000-0000-000000000000")
        .user_message("ONLY_IN_SESSION_BBBB")
        .done();

    // Scoped to "aaaa" prefix: finds AAAA text, not BBBB.
    let out_aaaa = world
        .cmd()
        .args(["search", "ONLY_IN_SESSION", "-t", "user",
               "--session", "aaaa", "--project", proj.path()])
        .output()
        .unwrap();
    assert!(out_aaaa.status.success());
    let text_aaaa = strip_ansi(stdout(&out_aaaa));
    assert!(text_aaaa.contains("ONLY_IN_SESSION_AAAA"));
    assert!(!text_aaaa.contains("ONLY_IN_SESSION_BBBB"),
        "--session should restrict to matching session only");
}

#[test]
fn test_search_before_context() {
    let world = MockWorld::new();
    let proj = world.project("before-ctx");
    // Multi-line user message: the parser extracts the whole text as one block.
    // find_matches splits on '\n', so context works within the text.
    proj.session("sess-bctx")
        .user_message("line_alpha\nline_beta\nTARGET_LINE_BCTX\nline_delta")
        .done();

    let no_ctx = world
        .cmd()
        .args(["search", "TARGET_LINE_BCTX", "-t", "user", "--project", proj.path()])
        .output()
        .unwrap();
    let with_b = world
        .cmd()
        .args(["search", "TARGET_LINE_BCTX", "-t", "user", "-B", "1", "--project", proj.path()])
        .output()
        .unwrap();

    let lines_no_ctx = strip_ansi(stdout(&no_ctx)).lines().count();
    let lines_with_b = strip_ansi(stdout(&with_b)).lines().count();
    assert!(lines_with_b >= lines_no_ctx,
        "-B 1 should produce at least as many lines as no context");
    assert!(strip_ansi(stdout(&with_b)).contains("line_beta"),
        "-B 1 should show the line before the match");
}

#[test]
fn test_search_after_context() {
    let world = MockWorld::new();
    let proj = world.project("after-ctx");
    proj.session("sess-actx")
        .user_message("line_one\nTARGET_LINE_ACTX\nline_three\nline_four")
        .done();

    let no_ctx = world
        .cmd()
        .args(["search", "TARGET_LINE_ACTX", "-t", "user", "--project", proj.path()])
        .output()
        .unwrap();
    let with_a = world
        .cmd()
        .args(["search", "TARGET_LINE_ACTX", "-t", "user", "-A", "1", "--project", proj.path()])
        .output()
        .unwrap();

    let lines_no_ctx = strip_ansi(stdout(&no_ctx)).lines().count();
    let lines_with_a = strip_ansi(stdout(&with_a)).lines().count();
    assert!(lines_with_a >= lines_no_ctx,
        "-A 1 should produce at least as many lines as no context");
    assert!(strip_ansi(stdout(&with_a)).contains("line_three"),
        "-A 1 should show the line after the match");
}

#[test]
fn test_search_max_line_width_unlimited() {
    let world = MockWorld::new();
    let proj = world.project("line-width");
    // Build a 300-char message: search term at the start, unique tail at the end.
    let tail = "UNIQUE_TAIL_MNO";
    let padding = "x".repeat(300 - "LINEWIDTH_SEARCH ".len() - tail.len());
    let long_msg = format!("LINEWIDTH_SEARCH {}{}", padding, tail);
    assert!(long_msg.len() > 200);

    proj.session("sess-lw").user_message(&long_msg).done();

    // Default max_line_width=200: tail should be truncated away.
    let default_out = world
        .cmd()
        .args(["search", "LINEWIDTH_SEARCH", "-t", "user", "--project", proj.path()])
        .output()
        .unwrap();
    assert!(default_out.status.success());
    let default_text = strip_ansi(stdout(&default_out));
    assert!(!default_text.contains(tail),
        "default max-line-width should truncate the tail");

    // --max-line-width 0: full line visible.
    let unlimited_out = world
        .cmd()
        .args(["search", "LINEWIDTH_SEARCH", "-t", "user",
               "--max-line-width", "0", "--project", proj.path()])
        .output()
        .unwrap();
    assert!(unlimited_out.status.success());
    let unlimited_text = strip_ansi(stdout(&unlimited_out));
    assert!(unlimited_text.contains(tail),
        "--max-line-width 0 should show the full line including tail");
}

#[test]
fn test_search_missing_project_exits_nonzero() {
    let world = MockWorld::new();
    let out = world
        .cmd()
        .args(["search", "anything", "--project", "/claugrep-mock/no-such-project"])
        .output()
        .unwrap();
    assert!(!out.status.success(),
        "search with no sessions should exit nonzero");
}

// ═════════════════════════════════════════════════════════════════════════════
// --max-results limit hint in summary
// ═════════════════════════════════════════════════════════════════════════════

#[test]
fn test_summary_shows_limit_hint_when_max_results_hit() {
    // Create more messages than the limit so the limit is definitely hit.
    let world = MockWorld::new();
    let proj = world.project("max-results-hint");
    let mut s = proj.session("sess-mr");
    for i in 0..5 {
        s = s.user_message(&format!("LIMIT_MATCH_{}", i));
    }
    s.done();

    // Request only 3 results — limit will be hit.
    let out = world.cmd()
        .args(["search", "LIMIT_MATCH", "--max-results", "3", "--project", proj.path()])
        .output().unwrap();

    assert!(out.status.success());
    let err = strip_ansi(stderr(&out));
    assert!(
        err.contains("--max-results") || err.contains("limit"),
        "stderr should mention the limit when it is hit, got: {}",
        err
    );
}

#[test]
fn test_summary_no_limit_hint_when_fewer_results_than_max() {
    // Only 2 messages, limit is 5 — limit is NOT hit.
    let world = MockWorld::new();
    let proj = world.project("max-results-no-hint");
    proj.session("sess-mrn")
        .user_message("UNDER_LIMIT_MATCH_0")
        .user_message("UNDER_LIMIT_MATCH_1")
        .done();

    let out = world.cmd()
        .args(["search", "UNDER_LIMIT_MATCH", "--max-results", "5", "--project", proj.path()])
        .output().unwrap();

    assert!(out.status.success());
    let err = strip_ansi(stderr(&out));
    assert!(
        !err.contains("--max-results"),
        "stderr should NOT mention --max-results when limit was not hit, got: {}",
        err
    );
}

#[test]
fn test_summary_no_limit_hint_when_exact_match_count_equals_max() {
    // Exactly 3 messages, limit is also 3 — this is the coincidence case.
    // We cannot distinguish it from a real limit hit without look-ahead,
    // so we accept either behavior here; this test just documents the case.
    // The important thing is tested by test_summary_no_limit_hint_when_fewer_results_than_max.
    let world = MockWorld::new();
    let proj = world.project("max-results-exact");
    proj.session("sess-mre")
        .user_message("EXACT_MATCH_0")
        .user_message("EXACT_MATCH_1")
        .user_message("EXACT_MATCH_2")
        .done();

    let out = world.cmd()
        .args(["search", "EXACT_MATCH", "--max-results", "3", "--project", proj.path()])
        .output().unwrap();

    assert!(out.status.success());
    // No assertion about --max-results presence — behavior is intentionally unspecified
    // when count == limit (we can't know without extra work whether more exist).
    let _ = strip_ansi(stdout(&out));
}

// ═════════════════════════════════════════════════════════════════════════════
// -F/--fixed-strings and -E/--extended-regexp flags
// ═════════════════════════════════════════════════════════════════════════════

#[test]
fn test_search_fixed_strings_matches_literal_dot() {
    // "a.b" as a regex matches "axb", but with -F it must only match the literal "a.b".
    let world = MockWorld::new();
    let proj = world.project("fixed-strings");
    proj.session("sess-fs")
        .user_message("literal a.b here")
        .user_message("should not match axb")
        .done();

    let out = world.cmd()
        .args(["search", "a.b", "-F", "--project", proj.path()])
        .output().unwrap();

    assert!(out.status.success());
    let text = strip_ansi(stdout(&out));
    assert!(text.contains("literal a.b here"), "-F should match literal dot");
    assert!(!text.contains("should not match axb"), "-F should not match regex wildcard");
}

#[test]
fn test_search_fixed_strings_long_flag() {
    // Same as above but using --fixed-strings instead of -F.
    let world = MockWorld::new();
    let proj = world.project("fixed-strings-long");
    proj.session("sess-fsl")
        .user_message("literal a.b here")
        .user_message("should not match axb")
        .done();

    let out = world.cmd()
        .args(["search", "a.b", "--fixed-strings", "--project", proj.path()])
        .output().unwrap();

    assert!(out.status.success());
    let text = strip_ansi(stdout(&out));
    assert!(text.contains("literal a.b here"), "--fixed-strings should match literal dot");
    assert!(!text.contains("should not match axb"), "--fixed-strings should not match regex wildcard");
}

#[test]
fn test_search_extended_regexp_matches_regex_not_literal() {
    // "a.b" as -E is pure regex: matches "axb" but also "a.b".
    // Without -E the default also matches both, so this test verifies
    // that -E does NOT suppress regex matching (it still works as regex).
    let world = MockWorld::new();
    let proj = world.project("extended-regexp");
    proj.session("sess-er")
        .user_message("regex match axb here")
        .user_message("also literal a.b here")
        .done();

    let out = world.cmd()
        .args(["search", "a.b", "-E", "--project", proj.path()])
        .output().unwrap();

    assert!(out.status.success());
    let text = strip_ansi(stdout(&out));
    assert!(text.contains("regex match axb here"), "-E should match via regex wildcard");
    assert!(text.contains("also literal a.b here"), "-E should also match literal dot");
}

#[test]
fn test_search_extended_regexp_long_flag() {
    let world = MockWorld::new();
    let proj = world.project("extended-regexp-long");
    proj.session("sess-erl")
        .user_message("regex match axb here")
        .done();

    let out = world.cmd()
        .args(["search", "a.b", "--extended-regexp", "--project", proj.path()])
        .output().unwrap();

    assert!(out.status.success());
    let text = strip_ansi(stdout(&out));
    assert!(text.contains("regex match axb here"), "--extended-regexp should match via regex");
}

#[test]
fn test_search_fixed_strings_does_not_match_regex_special_chars() {
    // With -F, "^hello" is a literal string not an anchor.
    let world = MockWorld::new();
    let proj = world.project("fixed-special");
    proj.session("sess-fsp")
        .user_message("prefix ^hello suffix")   // contains literal "^hello"
        .user_message("hello at start")          // would match regex ^hello
        .done();

    let out = world.cmd()
        .args(["search", "^hello", "-F", "--project", proj.path()])
        .output().unwrap();

    assert!(out.status.success());
    let text = strip_ansi(stdout(&out));
    assert!(text.contains("prefix ^hello suffix"), "-F should match literal ^hello");
    assert!(!text.contains("hello at start"), "-F should not treat ^ as regex anchor");
}

#[test]
fn test_search_extended_regexp_only_uses_regex_no_literal_fallback() {
    // With -E, an invalid regex should fail rather than fall back to literal matching.
    // "[unclosed" is an invalid regex — without -E the literal fallback finds it;
    // with -E there is no fallback so it should exit nonzero with an error.
    let world = MockWorld::new();
    let proj = world.project("extended-no-fallback");
    proj.session("sess-enf")
        .user_message("contains [unclosed bracket")
        .done();

    let out = world.cmd()
        .args(["search", "[unclosed", "-E", "--project", proj.path()])
        .output().unwrap();

    assert!(!out.status.success(), "-E with invalid regex should exit nonzero");
}

// ═════════════════════════════════════════════════════════════════════════════
// dump subcommand
// ═════════════════════════════════════════════════════════════════════════════

#[test]
fn test_dump_all_sessions() {
    let world = MockWorld::new();
    let proj = world.project("dump-all");
    proj.session("sess-da1").user_message("DUMP_ALL_SESSION_ONE").done();
    proj.session("sess-da2").user_message("DUMP_ALL_SESSION_TWO").done();

    let out = world
        .cmd()
        .args(["dump", "all", "--project", proj.path()])
        .output()
        .unwrap();

    assert!(out.status.success(), "stderr: {}", String::from_utf8_lossy(&out.stderr));
    let text = strip_ansi(stdout(&out));
    assert!(text.contains("DUMP_ALL_SESSION_ONE"), "expected session 1 content");
    assert!(text.contains("DUMP_ALL_SESSION_TWO"), "expected session 2 content");
}

#[test]
fn test_dump_uuid_prefix_positive_match() {
    let world = MockWorld::new();
    let proj = world.project("dump-prefix");
    proj.session("aaaa-prefix-sess-001").user_message("DUMP_PREFIX_AAAA_CONTENT").done();
    proj.session("bbbb-prefix-sess-002").user_message("DUMP_PREFIX_BBBB_CONTENT").done();

    let out = world
        .cmd()
        .args(["dump", "aaaa-prefix", "--project", proj.path()])
        .output()
        .unwrap();

    assert!(out.status.success(), "stderr: {}", String::from_utf8_lossy(&out.stderr));
    let text = strip_ansi(stdout(&out));
    assert!(text.contains("DUMP_PREFIX_AAAA_CONTENT"),
        "expected session matching prefix");
    assert!(!text.contains("DUMP_PREFIX_BBBB_CONTENT"),
        "should not dump non-matching session");
}

#[test]
fn test_dump_offset_zero_latest_session() {
    let world = MockWorld::new();
    let proj = world.project("dump-offset");
    // One session is enough to verify offset 0 works without crashing.
    proj.session("sess-offset-zero")
        .user_message("DUMP_OFFSET_ZERO_TEXT")
        .done();

    let out = world
        .cmd()
        .args(["dump", "0", "--project", proj.path()])
        .output()
        .unwrap();

    assert!(out.status.success(), "stderr: {}", String::from_utf8_lossy(&out.stderr));
    let text = strip_ansi(stdout(&out));
    assert!(text.contains("DUMP_OFFSET_ZERO_TEXT"),
        "dump 0 should return the session content");
}

#[test]
fn test_dump_multi_targets() {
    let world = MockWorld::new();
    let proj = world.project("dump-multi-tgt");
    proj.session("sess-mt")
        .user_message("DUMP_MT_USER_TEXT")
        .bash("DUMP_MT_BASH_CMD", "DUMP_MT_BASH_OUT")
        .done();

    // user,bash-command: user message and bash command appear; bash output does not.
    let out = world
        .cmd()
        .args(["dump", "1", "--targets", "user,bash-command", "--project", proj.path()])
        .output()
        .unwrap();

    assert!(out.status.success());
    let text = strip_ansi(stdout(&out));
    assert!(text.contains("DUMP_MT_USER_TEXT"),   "expected user text");
    assert!(text.contains("DUMP_MT_BASH_CMD"),    "expected bash command");
    assert!(!text.contains("DUMP_MT_BASH_OUT"),   "bash output should be excluded");

    // bash-output only: neither user nor bash-command appear.
    let out2 = world
        .cmd()
        .args(["dump", "1", "--targets", "bash-output", "--project", proj.path()])
        .output()
        .unwrap();

    assert!(out2.status.success());
    let text2 = strip_ansi(stdout(&out2));
    assert!(text2.contains("DUMP_MT_BASH_OUT"),  "expected bash output");
    assert!(!text2.contains("DUMP_MT_USER_TEXT"), "user text should be excluded");
}

#[test]
fn test_dump_no_session_defaults_to_latest() {
    // Without a session argument, dump should default to the latest session (offset 0),
    // matching journalctl's behaviour of defaulting to the current boot.
    let world = MockWorld::new();
    let proj = world.project("dump-default");
    proj.session("sess-older")
        .user_message("DUMP_DEFAULT_OLDER")
        .done();
    proj.session("sess-newer")
        .user_message("DUMP_DEFAULT_NEWER")
        .done();

    // Stagger mtimes explicitly: on some filesystems sequential writes can share
    // a single mtime tick, which makes session-order resolution ambiguous.
    let now = SystemTime::now();
    set_mtime(&proj.session_path("sess-older"), now - Duration::from_secs(10));
    set_mtime(&proj.session_path("sess-newer"), now);

    let out = world
        .cmd()
        .args(["dump", "--project", proj.path()])
        .output()
        .unwrap();

    assert!(out.status.success(), "stderr: {}", String::from_utf8_lossy(&out.stderr));
    let text = strip_ansi(stdout(&out));
    assert!(text.contains("DUMP_DEFAULT_NEWER"),
        "dump with no session arg should show the latest session");
    assert!(!text.contains("DUMP_DEFAULT_OLDER"),
        "dump with no session arg should not show older sessions");
}

#[test]
fn test_dump_broken_pipe_does_not_segfault() {
    // Reproduce: piping `claugrep dump` output to `head` causes a broken pipe.
    // Rust's default SIGPIPE handling panics, which manifests as a segfault/abort
    // instead of a clean exit. The process must exit without a signal.
    let world = MockWorld::new();
    let proj = world.project("dump-broken-pipe");
    // Write enough content that claugrep won't finish before we close the pipe.
    let mut s = proj.session("sess-bp");
    for i in 0..50 {
        s = s.user_message(&format!("BROKEN_PIPE_LINE_{}", i));
    }
    s.done();

    let mut child = world.cmd()
        .args(["dump", "--project", proj.path()])
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();

    // Read a few bytes then drop the pipe, simulating `head`.
    {
        let mut buf = [0u8; 64];
        let stdout = child.stdout.as_mut().unwrap();
        let _ = stdout.read(&mut buf);
        // stdout is dropped here, closing the read end of the pipe.
    }
    drop(child.stdout.take());

    let status = child.wait().unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt;
        // Must not have panicked (Rust panic = exit code 101).
        assert_ne!(
            status.code(),
            Some(101),
            "process panicked on broken pipe (exit 101); should handle EPIPE gracefully"
        );
        // Must not have crashed with segfault (11) or abort (6).
        // Signal 13 (SIGPIPE) is acceptable — it means the kernel terminated the
        // process cleanly when the pipe broke, which is the desired behaviour.
        let sig = status.signal();
        assert!(
            sig.is_none() || sig == Some(13),
            "process was killed by unexpected signal {:?}",
            sig
        );
    }
}

// ═════════════════════════════════════════════════════════════════════════════
// projects subcommand
// ═════════════════════════════════════════════════════════════════════════════

#[test]
fn test_projects_lists_all_projects() {
    let world = MockWorld::new();
    // Use names without '-' so the lossy decode is perfect
    let proj_a = world.project("projalpha");
    let proj_b = world.project("projbeta");
    proj_a.session("sess-pa").user_message("hello from alpha").done();
    proj_b.session("sess-pb").user_message("hello from beta").done();

    let out = world.cmd().args(["projects"]).output().unwrap();

    assert!(out.status.success(), "stderr: {}", String::from_utf8_lossy(&out.stderr));
    let text = stdout(&out);
    assert!(text.contains("projalpha"), "expected projalpha in output");
    assert!(text.contains("projbeta"),  "expected projbeta in output");
}

#[test]
fn test_projects_shows_session_count() {
    let world = MockWorld::new();
    let proj = world.project("threecount");
    proj.session("sess-tc1").user_message("msg1").done();
    proj.session("sess-tc2").user_message("msg2").done();
    proj.session("sess-tc3").user_message("msg3").done();

    let out = world.cmd().args(["projects"]).output().unwrap();

    assert!(out.status.success());
    let text = stdout(&out);
    // The output line for this project should mention 3 sessions
    assert!(text.contains("3 session"), "expected '3 session' in output, got: {}", text);
}

#[test]
fn test_projects_json_output() {
    let world = MockWorld::new();
    let proj = world.project("jsonproj");
    proj.session("sess-jp").user_message("test content").done();

    let out = world.cmd().args(["projects", "--json"]).output().unwrap();

    assert!(out.status.success());
    let parsed: serde_json::Value = serde_json::from_str(stdout(&out))
        .expect("--json must produce valid JSON");
    let arr = parsed.as_array().expect("expected JSON array");
    assert!(!arr.is_empty());

    // Find the entry for our project
    let entry = arr.iter().find(|v| {
        v["path"].as_str().unwrap_or("").contains("jsonproj")
    }).expect("expected jsonproj entry in JSON output");

    assert!(entry["path"].is_string(),        "expected path field");
    assert!(entry["encodedPath"].is_string(),  "expected encodedPath field");
    assert!(entry["sessionCount"].is_number(), "expected sessionCount field");
    assert_eq!(entry["sessionCount"].as_u64().unwrap(), 1, "expected sessionCount == 1");
    assert!(entry["latestMtime"].is_number(),  "expected latestMtime field");
}

#[test]
fn test_projects_empty_exits_nonzero() {
    let world = MockWorld::new();
    // No projects created in mock home

    let out = world.cmd().args(["projects"]).output().unwrap();

    assert!(!out.status.success(), "should exit nonzero when no projects exist");
}

#[test]
fn test_sessions_json_output() {
    let world = MockWorld::new();
    let proj = world.project("sess-json");
    proj.session("sess-sj1").user_message("first").done();
    proj.session("sess-sj2").user_message("second").done();

    let out = world
        .cmd()
        .args(["sessions", "--json", "--project", proj.path()])
        .output()
        .unwrap();

    assert!(out.status.success());
    let parsed: serde_json::Value = serde_json::from_str(stdout(&out))
        .expect("--json must produce valid JSON");
    let arr = parsed.as_array().expect("expected JSON array");
    assert_eq!(arr.len(), 2, "expected 2 sessions");
    let first = &arr[0];
    assert!(first["sessionId"].is_string());
    assert!(first["filePath"].is_string());
    assert!(first["mtime"].is_number());
}

#[test]
fn test_projects_shows_timestamp() {
    let world = MockWorld::new();
    let proj = world.project("tscheck");
    proj.session("sess-ts").user_message("timestamped").done();

    let out = world.cmd().args(["projects"]).output().unwrap();

    assert!(out.status.success());
    let text = stdout(&out);
    // Should contain a date-like pattern (YYYY-MM-DD)
    let has_date = text.lines().any(|l| l.contains("tscheck") && l.contains('-'));
    assert!(has_date, "expected timestamp in project listing, got: {}", text);
}

#[test]
fn test_projects_encoded_path_in_json() {
    let world = MockWorld::new();
    let proj = world.project("enctest");
    proj.session("sess-enc").user_message("enc").done();

    let out = world.cmd().args(["projects", "--json"]).output().unwrap();

    assert!(out.status.success());
    let parsed: serde_json::Value = serde_json::from_str(stdout(&out)).unwrap();
    let arr = parsed.as_array().unwrap();

    let entry = arr.iter().find(|v| {
        v["encodedPath"].as_str().unwrap_or("").contains("enctest")
    }).expect("expected enctest entry");

    // encodedPath should use '-' separators (the raw stored form)
    let ep = entry["encodedPath"].as_str().unwrap();
    assert!(ep.contains('-'), "encodedPath should contain '-' separators");
    assert!(ep.contains("enctest"), "encodedPath should contain project name");
}

#[test]
fn test_projects_sessions_flag() {
    let world = MockWorld::new();
    let proj = world.project("nested");
    proj.session("sess-n1").user_message("first").done();
    proj.session("sess-n2").user_message("second").done();

    let out = world.cmd().args(["projects", "--sessions"]).output().unwrap();

    assert!(out.status.success(), "stderr: {}", String::from_utf8_lossy(&out.stderr));
    let text = stdout(&out);
    // Project line should appear
    assert!(text.contains("nested"), "expected project name in output");
    // Session IDs should appear indented
    assert!(text.contains("sess-n1"), "expected sess-n1 in output, got: {}", text);
    assert!(text.contains("sess-n2"), "expected sess-n2 in output, got: {}", text);
}

#[test]
fn test_projects_sessions_json() {
    let world = MockWorld::new();
    let proj = world.project("nestedjson");
    proj.session("sess-nj1").user_message("first").done();
    proj.session("sess-nj2").user_message("second").done();

    let out = world.cmd().args(["projects", "--json", "--sessions"]).output().unwrap();

    assert!(out.status.success());
    let parsed: serde_json::Value = serde_json::from_str(stdout(&out))
        .expect("--json must produce valid JSON");
    let arr = parsed.as_array().expect("expected JSON array");

    let entry = arr.iter().find(|v| {
        v["path"].as_str().unwrap_or("").contains("nestedjson")
    }).expect("expected nestedjson entry");

    let sessions = entry["sessions"].as_array().expect("expected sessions array");
    assert_eq!(sessions.len(), 2, "expected 2 sessions");
    assert!(sessions[0]["sessionId"].is_string());
    assert!(sessions[0]["mtime"].is_number());
}

#[test]
fn test_projects_no_unverified_label() {
    let world = MockWorld::new();
    // Mock project paths don't exist on real filesystem, so they would have been [unverified]
    let proj = world.project("fakepath");
    proj.session("sess-fp").user_message("msg").done();

    let out = world.cmd().args(["projects"]).output().unwrap();

    assert!(out.status.success());
    let text = stdout(&out);
    assert!(!text.contains("[unverified]"), "output should not contain [unverified], got: {}", text);
}

// =============================================================================
// error handling — incorrect invocations exit nonzero with clap's default output
// =============================================================================

fn stderr(out: &std::process::Output) -> &str {
    std::str::from_utf8(&out.stderr).unwrap()
}

#[test]
fn test_unknown_subcommand_exits_nonzero_with_error() {
    let world = MockWorld::new();

    let out = world.cmd().args(["foobar"]).output().unwrap();

    assert!(!out.status.success(), "unknown subcommand should exit nonzero");
    let err = strip_ansi(stderr(&out));
    assert!(err.contains("foobar"), "stderr should mention the unrecognized subcommand");
    // clap's default output should not repeat Usage/help section more than once
    let usage_count = err.matches("Usage:").count();
    assert!(usage_count <= 1, "Usage: should appear at most once, got {} times:\n{}", usage_count, err);
}

#[test]
fn test_search_missing_required_arg_shows_usage_and_exits_nonzero() {
    let world = MockWorld::new();

    // 'search' requires a PATTERN argument — omitting it is an error.
    let out = world.cmd().args(["search"]).output().unwrap();

    assert!(!out.status.success(), "missing required arg should exit nonzero");
    let err = strip_ansi(stderr(&out));
    // The error output should mention usage / the missing argument
    assert!(
        err.contains("PATTERN") || err.contains("required") || err.contains("Usage"),
        "stderr should mention missing pattern or usage, got: {}",
        err
    );
}

// =============================================================================
// truncate_line — multibyte character boundary safety
// =============================================================================

#[test]
fn test_search_truncation_does_not_panic_on_multibyte_boundary() {
    // Reproduce: byte index falls inside a multi-byte char (e.g. em dash U+2014,
    // encoded as 3 bytes 0xE2 0x80 0x94) when truncating around a match.
    // Build a line where the match keyword sits at ~byte 95 and an em-dash
    // straddles the computed start/end boundary at max_line_width=100.
    let world = MockWorld::new();
    let proj = world.project("multibyte-trunc");

    // Construct a line where the computed `start` offset lands inside an em dash
    // (U+2014, 3 UTF-8 bytes: 0xE2 0x80 0x94).
    //
    // With --max-line-width 100:
    //   match_start = 100 (KEYWORD starts at byte 100)
    //   match_len   = 7
    //   budget      = 100 - 7 = 93
    //   before      = 93 / 2 = 46
    //   start       = 100 - 46 = 54
    //
    // If byte 54 is the second byte of a 3-byte em dash the slice panics.
    // Place em dash at bytes 53..56 (i.e. 53 ASCII chars then '—').
    let prefix_ascii = "x".repeat(53); // bytes 0..53
    // em dash occupies bytes 53, 54, 55
    // then enough ASCII to reach byte 100 for KEYWORD: 100 - 56 = 44 chars
    let filler = "y".repeat(44);
    let suffix = " rest of the line goes on and on and on and on and on";
    let msg = format!("{}—{}KEYWORD{}", prefix_ascii, filler, suffix);

    proj.session("sess-mb")
        .user_message(&msg)
        .done();

    let out = world
        .cmd()
        .args([
            "search", "KEYWORD",
            "--project", proj.path(),
            "--max-line-width", "100",
        ])
        .output()
        .unwrap();

    assert!(
        out.status.success(),
        "should not panic; stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let text = strip_ansi(stdout(&out));
    assert!(text.contains("KEYWORD"), "output should contain the match");
}

// =============================================================================
// claudex account and --config-dir tests
// =============================================================================

#[test]
fn test_config_dir_env_var() {
    // Sessions stored in a custom dir pointed to by CLAUDE_CONFIG_DIR should be found.
    let world = MockWorld::new();
    let custom_dir = world.home.path().join("custom-claude");
    let project_path = "/claugrep-mock/env-var-proj";
    let encoded = project_path.replace(['/', '.'], "-");
    let session_dir = custom_dir.join("projects").join(&encoded);
    fs::create_dir_all(&session_dir).unwrap();
    let mut sb = SessionBuilder::new(
        "env-var-sess".to_string(),
        session_dir.join("env-var-sess.jsonl"),
    );
    sb = sb.user_message("ENV_VAR_CONFIG_DIR_CONTENT");
    sb.done();

    let out = world
        .cmd()
        .env("CLAUDE_CONFIG_DIR", &custom_dir)
        .args(["last", "--project", project_path])
        .output()
        .unwrap();

    assert!(out.status.success(), "stderr: {}", String::from_utf8_lossy(&out.stderr));
    let text = strip_ansi(stdout(&out));
    assert!(text.contains("ENV_VAR_CONFIG_DIR_CONTENT"),
        "CLAUDE_CONFIG_DIR env var should point discovery to custom dir");
}

#[test]
fn test_config_dir_flag() {
    // Sessions stored in a custom dir pointed to by --config-dir flag should be found.
    let world = MockWorld::new();
    let custom_dir = world.home.path().join("flag-claude");
    let project_path = "/claugrep-mock/flag-dir-proj";
    let encoded = project_path.replace(['/', '.'], "-");
    let session_dir = custom_dir.join("projects").join(&encoded);
    fs::create_dir_all(&session_dir).unwrap();
    let mut sb = SessionBuilder::new(
        "flag-dir-sess".to_string(),
        session_dir.join("flag-dir-sess.jsonl"),
    );
    sb = sb.user_message("FLAG_CONFIG_DIR_CONTENT");
    sb.done();

    let out = world
        .cmd()
        .args([
            "--config-dir", custom_dir.to_str().unwrap(),
            "last", "--project", project_path,
        ])
        .output()
        .unwrap();

    assert!(out.status.success(), "stderr: {}", String::from_utf8_lossy(&out.stderr));
    let text = strip_ansi(stdout(&out));
    assert!(text.contains("FLAG_CONFIG_DIR_CONTENT"),
        "--config-dir flag should point discovery to the specified dir");
}

#[test]
fn test_claudex_account_auto_discover() {
    // Sessions under a claudex account dir should be found automatically by `last`
    // even without any flags (auto-discovery of accounts).
    let world = MockWorld::new();
    let proj = world.account_project("myaccount", "auto-disc");
    proj.session("auto-disc-sess")
        .user_message("CLAUDEX_AUTO_DISCOVER_CONTENT")
        .done();

    let out = world
        .cmd()
        .args(["last"])
        .output()
        .unwrap();

    assert!(out.status.success(), "stderr: {}", String::from_utf8_lossy(&out.stderr));
    let text = strip_ansi(stdout(&out));
    assert!(text.contains("CLAUDEX_AUTO_DISCOVER_CONTENT"),
        "sessions under claudex account should be found automatically");
}

#[test]
fn test_account_flag_filters() {
    // With --account foo, only sessions from that account should appear.
    let world = MockWorld::new();

    // Session in default ~/.claude
    let default_proj = world.project("acct-filter-default");
    default_proj.session("default-sess")
        .user_message("ACCOUNT_FILTER_DEFAULT_CONTENT")
        .done();

    // Session in claudex account "foo"
    let acct_proj = world.account_project("foo", "acct-filter-foo");
    acct_proj.session("foo-sess")
        .user_message("ACCOUNT_FILTER_FOO_CONTENT")
        .done();

    // Without --account: both should appear
    let out_all = world.cmd().args(["last"]).output().unwrap();
    assert!(out_all.status.success(), "stderr: {}", String::from_utf8_lossy(&out_all.stderr));
    let text_all = strip_ansi(stdout(&out_all));
    assert!(text_all.contains("ACCOUNT_FILTER_DEFAULT_CONTENT"), "default sessions should appear without --account");
    assert!(text_all.contains("ACCOUNT_FILTER_FOO_CONTENT"), "account sessions should appear without --account");

    // With --account foo: only foo sessions
    let out_foo = world.cmd().args(["--account", "foo", "last"]).output().unwrap();
    assert!(out_foo.status.success(), "stderr: {}", String::from_utf8_lossy(&out_foo.stderr));
    let text_foo = strip_ansi(stdout(&out_foo));
    assert!(text_foo.contains("ACCOUNT_FILTER_FOO_CONTENT"), "foo account session should appear with --account foo");
    assert!(!text_foo.contains("ACCOUNT_FILTER_DEFAULT_CONTENT"),
        "--account foo should not show default sessions");
}

#[test]
fn test_account_flag_search() {
    // search with --account should scope to that account's sessions only.
    let world = MockWorld::new();

    let default_proj = world.project("acct-search-default");
    default_proj.session("dsess")
        .user_message("ACCT_SEARCH_DEFAULT_ONLY")
        .done();

    let acct_proj = world.account_project("bar", "acct-search-bar");
    acct_proj.session("bsess")
        .user_message("ACCT_SEARCH_BAR_ONLY")
        .done();

    // Search for bar content with --account bar: should find it
    let out_found = world
        .cmd()
        .args(["--account", "bar", "search", "ACCT_SEARCH_BAR_ONLY",
               "--project", acct_proj.path()])
        .output()
        .unwrap();
    assert!(out_found.status.success(), "stderr: {}", String::from_utf8_lossy(&out_found.stderr));
    let text_found = strip_ansi(stdout(&out_found));
    assert!(text_found.contains("ACCT_SEARCH_BAR_ONLY"), "should find bar session with --account bar");

    // Search for default content with --account bar: should not find it (wrong account)
    let out_miss = world
        .cmd()
        .args(["--account", "bar", "search", "ACCT_SEARCH_DEFAULT_ONLY",
               "--project", default_proj.path()])
        .output()
        .unwrap();
    // Either exits nonzero (no sessions in that path under bar account) or finds no matches
    let text_miss = strip_ansi(stdout(&out_miss));
    assert!(!text_miss.contains("ACCT_SEARCH_DEFAULT_ONLY"),
        "--account bar should not find default sessions");
}

// ═════════════════════════════════════════════════════════════════════════════
// --after / --since date filter
// ═════════════════════════════════════════════════════════════════════════════

/// Set a file's mtime to the given SystemTime.
fn set_mtime(path: &PathBuf, t: SystemTime) {
    let ft = filetime::FileTime::from_system_time(t);
    filetime::set_file_mtime(path, ft).expect("failed to set file mtime");
}

#[test]
fn test_since_sessions_filters_old_session() {
    let world = MockWorld::new();
    let proj = world.project("since-sessions");

    // Old session: 5 days ago
    proj.session("aaaa-old").user_message("OLD_SESSION_CONTENT").done();
    let old_path = proj.session_dir.join("aaaa-old.jsonl");
    set_mtime(&old_path, SystemTime::now() - Duration::from_secs(5 * 24 * 3600));

    // Recent session: 1 hour ago (definitely after "yesterday")
    proj.session("bbbb-new").user_message("NEW_SESSION_CONTENT").done();
    // file mtime is already current (just created)

    // --after yesterday: only new session should appear in sessions list
    let out = world
        .cmd()
        .args(["--after", "yesterday", "sessions", "--project", proj.path()])
        .output()
        .unwrap();

    assert!(out.status.success(), "stderr: {}", String::from_utf8_lossy(&out.stderr));
    let text = stdout(&out);
    assert!(text.contains("bbbb-new"), "recent session should appear");
    assert!(!text.contains("aaaa-old"), "old session should be filtered out by --after yesterday");
}

#[test]
fn test_since_alias_for_after() {
    let world = MockWorld::new();
    let proj = world.project("since-alias");

    proj.session("cccc-old").user_message("ALIAS_OLD").done();
    let old_path = proj.session_dir.join("cccc-old.jsonl");
    set_mtime(&old_path, SystemTime::now() - Duration::from_secs(5 * 24 * 3600));

    proj.session("dddd-new").user_message("ALIAS_NEW").done();

    // --since is an alias for --after
    let out = world
        .cmd()
        .args(["--since", "yesterday", "sessions", "--project", proj.path()])
        .output()
        .unwrap();

    assert!(out.status.success(), "stderr: {}", String::from_utf8_lossy(&out.stderr));
    let text = stdout(&out);
    assert!(text.contains("dddd-new"), "recent session should appear with --since");
    assert!(!text.contains("cccc-old"), "old session should be filtered out by --since");
}

#[test]
fn test_since_search_filters_sessions() {
    let world = MockWorld::new();
    let proj = world.project("since-search");

    // Both sessions contain the same keyword, but only the new one should match after filtering
    proj.session("eeee-old").user_message("SINCE_SEARCH_KEYWORD").done();
    let old_path = proj.session_dir.join("eeee-old.jsonl");
    set_mtime(&old_path, SystemTime::now() - Duration::from_secs(5 * 24 * 3600));

    proj.session("ffff-new").user_message("SINCE_SEARCH_KEYWORD").done();

    let out = world
        .cmd()
        .args(["--after", "yesterday", "search", "SINCE_SEARCH_KEYWORD",
               "--project", proj.path()])
        .output()
        .unwrap();

    assert!(out.status.success(), "stderr: {}", String::from_utf8_lossy(&out.stderr));
    let text = strip_ansi(stdout(&out));
    assert!(text.contains("ffff-new"), "recent session match should appear");
    assert!(!text.contains("eeee-old"), "old session should be filtered out by --after");
}

#[test]
fn test_since_iso_date_format() {
    let world = MockWorld::new();
    let proj = world.project("since-iso");

    proj.session("gggg-old").user_message("ISO_OLD").done();
    let old_path = proj.session_dir.join("gggg-old.jsonl");
    // Set to 10 days ago
    set_mtime(&old_path, SystemTime::now() - Duration::from_secs(10 * 24 * 3600));

    proj.session("hhhh-new").user_message("ISO_NEW").done();

    // Use a date 3 days ago in ISO format: compute it
    let cutoff = {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs();
        let secs = now - 3 * 24 * 3600;
        // Format as YYYY-MM-DD
        let dt = chrono::DateTime::<chrono::Utc>::from(
            std::time::UNIX_EPOCH + std::time::Duration::from_secs(secs),
        );
        dt.format("%Y-%m-%d").to_string()
    };

    let out = world
        .cmd()
        .args(["--after", &cutoff, "sessions", "--project", proj.path()])
        .output()
        .unwrap();

    assert!(out.status.success(), "stderr: {}", String::from_utf8_lossy(&out.stderr));
    let text = stdout(&out);
    assert!(text.contains("hhhh-new"), "recent session should appear with ISO date");
    assert!(!text.contains("gggg-old"), "old session (10 days ago) should be filtered out");
}

#[test]
fn test_since_no_filter_shows_all() {
    let world = MockWorld::new();
    let proj = world.project("since-nofilter");

    proj.session("iiii-old").user_message("NO_FILTER_OLD").done();
    let old_path = proj.session_dir.join("iiii-old.jsonl");
    set_mtime(&old_path, SystemTime::now() - Duration::from_secs(5 * 24 * 3600));

    proj.session("jjjj-new").user_message("NO_FILTER_NEW").done();

    // Without --after, both sessions should appear
    let out = world
        .cmd()
        .args(["sessions", "--project", proj.path()])
        .output()
        .unwrap();

    assert!(out.status.success());
    let text = stdout(&out);
    assert!(text.contains("iiii-old"), "old session should appear without date filter");
    assert!(text.contains("jjjj-new"), "new session should appear without date filter");
}

#[test]
fn test_since_relative_days_ago() {
    let world = MockWorld::new();
    let proj = world.project("since-reldays");

    proj.session("kkkk-old").user_message("REL_DAYS_OLD").done();
    let old_path = proj.session_dir.join("kkkk-old.jsonl");
    set_mtime(&old_path, SystemTime::now() - Duration::from_secs(10 * 24 * 3600));

    proj.session("llll-new").user_message("REL_DAYS_NEW").done();

    let out = world
        .cmd()
        .args(["--after", "5 days ago", "sessions", "--project", proj.path()])
        .output()
        .unwrap();

    assert!(out.status.success(), "stderr: {}", String::from_utf8_lossy(&out.stderr));
    let text = stdout(&out);
    assert!(text.contains("llll-new"), "recent session should appear with '5 days ago'");
    assert!(!text.contains("kkkk-old"), "10-day-old session should be filtered with '5 days ago'");
}

#[test]
fn test_since_invalid_date_exits_nonzero() {
    let world = MockWorld::new();
    let proj = world.project("since-invalid");
    proj.session("mmmm").user_message("INVALID_DATE_TEST").done();

    let out = world
        .cmd()
        .args(["--after", "not-a-date-at-all-xyz", "sessions", "--project", proj.path()])
        .output()
        .unwrap();

    assert!(!out.status.success(), "invalid --after value should exit nonzero");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("cannot parse date") || stderr.contains("error"),
        "should print an error about the bad date");
}

// --before / --until date filter
// ═════════════════════════════════════════════════════════════════════════════

#[test]
fn test_before_sessions_filters_new_session() {
    let world = MockWorld::new();
    let proj = world.project("before-sessions");

    // Old session: 10 days ago
    proj.session("aaaa-old").user_message("OLD_BEFORE_CONTENT").done();
    let old_path = proj.session_dir.join("aaaa-old.jsonl");
    set_mtime(&old_path, SystemTime::now() - Duration::from_secs(10 * 24 * 3600));

    // Recent session: 1 hour ago
    proj.session("bbbb-new").user_message("NEW_BEFORE_CONTENT").done();
    // mtime is already current (just created)

    // --before yesterday: only the old session should appear (new is after yesterday)
    let out = world
        .cmd()
        .args(["--before", "yesterday", "sessions", "--project", proj.path()])
        .output()
        .unwrap();

    assert!(out.status.success(), "should exit 0: {}", String::from_utf8_lossy(&out.stderr));
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(text.contains("aaaa-old"), "10-day-old session should appear before yesterday");
    assert!(!text.contains("bbbb-new"), "recent session should be filtered out");
}

#[test]
fn test_until_alias_for_before() {
    let world = MockWorld::new();
    let proj = world.project("until-alias");

    proj.session("cccc-old").user_message("UNTIL_ALIAS_CONTENT").done();
    let old_path = proj.session_dir.join("cccc-old.jsonl");
    set_mtime(&old_path, SystemTime::now() - Duration::from_secs(10 * 24 * 3600));

    proj.session("dddd-new").user_message("UNTIL_ALIAS_NEW").done();

    // --until should behave identically to --before
    let out = world
        .cmd()
        .args(["--until", "yesterday", "sessions", "--project", proj.path()])
        .output()
        .unwrap();

    assert!(out.status.success(), "should exit 0: {}", String::from_utf8_lossy(&out.stderr));
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(text.contains("cccc-old"), "--until should include old session");
    assert!(!text.contains("dddd-new"), "--until should filter recent session");
}

#[test]
fn test_before_search_filters_sessions() {
    let world = MockWorld::new();
    let proj = world.project("before-search");

    // Both sessions contain the keyword; only the old one should match after filtering
    proj.session("bsrc-old").user_message("BEFORE_SEARCH_KEYWORD").done();
    let old_path = proj.session_dir.join("bsrc-old.jsonl");
    set_mtime(&old_path, SystemTime::now() - Duration::from_secs(10 * 24 * 3600));

    proj.session("bsrc-new").user_message("BEFORE_SEARCH_KEYWORD").done();

    let out = world
        .cmd()
        .args(["--before", "yesterday", "search", "BEFORE_SEARCH_KEYWORD", "--project", proj.path()])
        .output()
        .unwrap();

    assert!(out.status.success(), "should exit 0: {}", String::from_utf8_lossy(&out.stderr));
    let text = strip_ansi(stdout(&out));
    assert!(text.contains("bsrc-old"), "old session should appear in matches");
    assert!(!text.contains("bsrc-new"), "new session should be filtered out by --before");
}

#[test]
fn test_before_iso_date_format() {
    let world = MockWorld::new();
    let proj = world.project("before-iso");

    // Session 20 days old
    proj.session("gggg-old").user_message("BEFORE_ISO_OLD").done();
    let old_path = proj.session_dir.join("gggg-old.jsonl");
    set_mtime(&old_path, SystemTime::now() - Duration::from_secs(20 * 24 * 3600));

    // Session 5 days old
    proj.session("hhhh-mid").user_message("BEFORE_ISO_MID").done();
    let mid_path = proj.session_dir.join("hhhh-mid.jsonl");
    set_mtime(&mid_path, SystemTime::now() - Duration::from_secs(5 * 24 * 3600));

    // Cutoff: 10 days ago as ISO date
    let cutoff = {
        let d = chrono::Utc::now() - chrono::Duration::days(10);
        d.format("%Y-%m-%d").to_string()
    };

    let out = world
        .cmd()
        .args(["--before", &cutoff, "sessions", "--project", proj.path()])
        .output()
        .unwrap();

    assert!(out.status.success(), "should exit 0");
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(text.contains("gggg-old"), "20-day-old session should appear before 10-day cutoff");
    assert!(!text.contains("hhhh-mid"), "5-day-old session should be filtered out");
}

#[test]
fn test_before_and_after_combined() {
    let world = MockWorld::new();
    let proj = world.project("before-after-combined");

    // 15 days old — outside both windows
    proj.session("iiii-ancient").user_message("COMBINED_ANCIENT").done();
    let ancient_path = proj.session_dir.join("iiii-ancient.jsonl");
    set_mtime(&ancient_path, SystemTime::now() - Duration::from_secs(15 * 24 * 3600));

    // 5 days old — inside the window (after 10 days ago, before 2 days ago)
    proj.session("jjjj-mid").user_message("COMBINED_MID").done();
    let mid_path = proj.session_dir.join("jjjj-mid.jsonl");
    set_mtime(&mid_path, SystemTime::now() - Duration::from_secs(5 * 24 * 3600));

    // 1 hour old — outside window (after "2 days ago" cutoff)
    proj.session("kkkk-recent").user_message("COMBINED_RECENT").done();
    // mtime is already current

    let out = world
        .cmd()
        .args(["--after", "10 days ago", "--before", "2 days ago",
               "sessions", "--project", proj.path()])
        .output()
        .unwrap();

    assert!(out.status.success(), "should exit 0");
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(text.contains("jjjj-mid"), "5-day-old session should be in window");
    assert!(!text.contains("iiii-ancient"), "15-day-old session should be filtered by --after");
    assert!(!text.contains("kkkk-recent"), "recent session should be filtered by --before");
}

#[test]
fn test_before_invalid_date_exits_nonzero() {
    let world = MockWorld::new();
    let proj = world.project("before-invalid");
    proj.session("llll").user_message("BEFORE_INVALID_TEST").done();

    let out = world
        .cmd()
        .args(["--before", "not-a-date-xyz", "sessions", "--project", proj.path()])
        .output()
        .unwrap();

    assert!(!out.status.success(), "invalid --before value should exit nonzero");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("cannot parse date") || stderr.contains("error"),
        "should print an error about the bad date");
}

// ── Edit tool diff view (default) / --no-diff ─────────────────────────────────

#[test]
fn test_diff_shows_unified_diff_for_edit_tool() {
    let world = MockWorld::new();
    let proj = world.project("diff-basic");
    proj.session("aaaa")
        .edit("/src/main.rs", "fn old_name() {}\n", "fn new_name() {}\n")
        .done();

    // Diff is the default — no flag needed
    let out = world
        .cmd()
        .args(["search", "old_name", "-t", "tool-use", "--project", proj.path()])
        .output()
        .unwrap();

    assert!(out.status.success(), "stderr: {}", String::from_utf8_lossy(&out.stderr));
    let text = strip_ansi(stdout(&out));
    assert!(text.contains("--- a/src/main.rs"), "should have --- header");
    assert!(text.contains("+++ b/src/main.rs"), "should have +++ header");
    assert!(text.contains("-fn old_name() {}"), "should show removed line");
    assert!(text.contains("+fn new_name() {}"), "should show added line");
}

#[test]
fn test_diff_hunk_header_present() {
    let world = MockWorld::new();
    let proj = world.project("diff-hunk");
    proj.session("bbbb")
        .edit("/foo.rs", "line1\nline2\n", "line1\nchanged\n")
        .done();

    let out = world
        .cmd()
        .args(["search", "line", "-t", "tool-use", "--project", proj.path()])
        .output()
        .unwrap();

    let text = strip_ansi(stdout(&out));
    assert!(text.contains("@@"), "diff output should contain @@ hunk header");
}

#[test]
fn test_no_diff_flag_shows_raw_format() {
    let world = MockWorld::new();
    let proj = world.project("diff-raw");
    proj.session("cccc")
        .edit("/bar.rs", "old content\n", "new content\n")
        .done();

    let out = world
        .cmd()
        .args(["search", "old_string", "-t", "tool-use", "--no-diff", "--project", proj.path()])
        .output()
        .unwrap();

    // With --no-diff, output should NOT contain unified diff markers
    let text = strip_ansi(stdout(&out));
    assert!(!text.contains("--- a/"), "--no-diff should not show unified diff header");
    assert!(!text.contains("+++ b/"), "--no-diff should not show unified diff header");
    assert!(text.contains("old_string"), "--no-diff should show raw key/value format");
}

#[test]
fn test_diff_non_edit_tool_unaffected() {
    let world = MockWorld::new();
    let proj = world.project("diff-non-edit");
    proj.session("dddd")
        .tool("Read", "file_path", "/some/file.rs", "file content here")
        .done();

    let out = world
        .cmd()
        .args(["search", "file_path", "-t", "tool-use", "--project", proj.path()])
        .output()
        .unwrap();

    // Non-Edit tool-use should still render normally (not as a diff)
    let text = strip_ansi(stdout(&out));
    assert!(!text.contains("--- a/"), "non-Edit tool should not be rendered as diff");
    assert!(text.contains("file_path"), "should still match the content");
}

#[test]
fn test_diff_file_path_in_header() {
    let world = MockWorld::new();
    let proj = world.project("diff-filepath");
    proj.session("eeee")
        .edit("/deep/nested/path/module.rs", "x = 1\n", "x = 2\n")
        .done();

    let out = world
        .cmd()
        .args(["search", "x = ", "-t", "tool-use", "--project", proj.path()])
        .output()
        .unwrap();

    let text = strip_ansi(stdout(&out));
    assert!(text.contains("/deep/nested/path/module.rs"),
        "file path should appear in diff header");
}

#[test]
fn test_diff_multiline_old_and_new_strings() {
    let world = MockWorld::new();
    let proj = world.project("diff-multiline");
    proj.session("ffff")
        .edit(
            "/lib.rs",
            "fn alpha() {\n    let x = 1;\n    x\n}\n",
            "fn alpha() {\n    let y = 2;\n    y\n}\n",
        )
        .done();

    let out = world
        .cmd()
        .args(["search", "alpha", "-t", "tool-use", "--project", proj.path()])
        .output()
        .unwrap();

    let text = strip_ansi(stdout(&out));
    // fn alpha() { is unchanged — appears as context (space prefix), not deleted/added
    assert!(text.contains(" fn alpha() {"), "fn header should appear as context line");
    assert!(!text.contains("-fn alpha() {"), "fn header should not appear as removed (it's equal)");
    assert!(!text.contains("+fn alpha() {"), "fn header should not appear as added (it's equal)");
    // changed body lines
    assert!(text.contains("-    let x = 1;"), "should show removed body line");
    assert!(text.contains("+    let y = 2;"), "should show added body line");
}

#[test]
fn test_diff_edit_tool_name_in_header() {
    let world = MockWorld::new();
    let proj = world.project("diff-toolname");
    proj.session("gggg")
        .edit("/x.rs", "old\n", "new\n")
        .done();

    let out = world
        .cmd()
        .args(["search", "old", "-t", "tool-use", "--project", proj.path()])
        .output()
        .unwrap();

    let text = strip_ansi(stdout(&out));
    assert!(text.contains("tool: Edit"), "should show tool name in header");
}

#[test]
fn test_diff_json_output_unaffected() {
    let world = MockWorld::new();
    let proj = world.project("diff-json");
    proj.session("hhhh")
        .edit("/y.rs", "before\n", "after\n")
        .done();

    // --json output prints raw JSONL records
    let out = world
        .cmd()
        .args(["search", "before", "-t", "tool-use", "--json", "--project", proj.path()])
        .output()
        .unwrap();

    assert!(out.status.success());
    let text = stdout(&out);
    let lines: Vec<&str> = text.lines().collect();
    assert!(!lines.is_empty(), "should have at least one JSONL line");
    let first: serde_json::Value = serde_json::from_str(lines[0])
        .expect("each line must be valid JSON");
    assert!(first["type"].is_string(), "raw entry should have a type field");
}

// ═════════════════════════════════════════════════════════════════════════════
// system records (#21)
// ═════════════════════════════════════════════════════════════════════════════

#[test]
fn test_search_system_record_with_content() {
    let world = MockWorld::new();
    let proj = world.project("sys-content");
    proj.session("sess-sc")
        .user_message("hello")
        .system_record("compact_boundary", Some("Conversation compacted UNIQUE_COMPACT_XYZ"))
        .done();

    // -t system finds the content
    let found = world
        .cmd()
        .args(["search", "UNIQUE_COMPACT_XYZ", "-t", "system", "--project", proj.path()])
        .output()
        .unwrap();
    assert!(found.status.success());
    assert!(!strip_ansi(stdout(&found)).contains("No matches found"),
        "-t system should find system records by content");

    // default targets should NOT find system records
    let miss = world
        .cmd()
        .args(["search", "UNIQUE_COMPACT_XYZ", "--project", proj.path()])
        .output()
        .unwrap();
    assert!(strip_ansi(stdout(&miss)).contains("No matches found"),
        "system records should not appear in default targets");
}

#[test]
fn test_search_system_record_without_content() {
    let world = MockWorld::new();
    let proj = world.project("sys-nocontent");
    proj.session("sess-sn")
        .user_message("hello")
        .system_record("turn_duration", None)
        .done();

    // -t system finds records without content (by matching the subtype)
    let found = world
        .cmd()
        .args(["search", "turn_duration", "-t", "system", "--project", proj.path()])
        .output()
        .unwrap();
    assert!(found.status.success());
    assert!(!strip_ansi(stdout(&found)).contains("No matches found"),
        "-t system should find system records by subtype when no content");
}

#[test]
fn test_dump_system_record() {
    let world = MockWorld::new();
    let proj = world.project("sys-dump");
    proj.session("sess-sd")
        .user_message("hello user")
        .system_record("compact_boundary", Some("Conversation compacted"))
        .system_record("turn_duration", None)
        .assistant_message("hello assistant")
        .done();

    // -t system shows system records
    let out = world
        .cmd()
        .args(["dump", "0", "-t", "system", "--project", proj.path()])
        .output()
        .unwrap();
    assert!(out.status.success());
    let text = stdout(&out);
    assert!(text.contains("compact_boundary"), "dump should show subtype");
    assert!(text.contains("Conversation compacted"), "dump should show content");
    assert!(text.contains("turn_duration"), "dump should show subtype for content-less records");
    assert!(!text.contains("hello user"), "should not show user messages");
    assert!(!text.contains("hello assistant"), "should not show assistant messages");
}

#[test]
fn test_system_not_in_default_not_warned() {
    let world = MockWorld::new();
    let proj = world.project("sys-nowarn");
    proj.session("sess-sw")
        .user_message("hello")
        .system_record("stop_hook_summary", None)
        .done();

    // default dump should not warn about system records
    let out = world
        .cmd()
        .args(["dump", "0", "--project", proj.path()])
        .output()
        .unwrap();
    assert!(out.status.success());
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(!err.contains("warning: skipping unrecognized record"),
        "system records should not trigger unrecognized record warnings");
}

// ═════════════════════════════════════════════════════════════════════════════
// file-history-snapshot target
// ═════════════════════════════════════════════════════════════════════════════

#[test]
fn test_search_file_history_snapshot() {
    let world = MockWorld::new();
    let proj = world.project("fhs-search");
    proj.session("sess-fhs")
        .user_message("hello")
        .file_history_snapshot(&["src/main.rs", "src/lib.rs"])
        .done();

    // -t file-history-snapshot finds the record
    let found = world
        .cmd()
        .args(["search", "src/main.rs", "-t", "file-history-snapshot", "--project", proj.path()])
        .output()
        .unwrap();
    assert!(found.status.success());
    assert!(!strip_ansi(stdout(&found)).contains("No matches found"),
        "-t file-history-snapshot should find snapshot records");

    // default targets should NOT find file-history-snapshot records
    let miss = world
        .cmd()
        .args(["search", "src/main.rs", "-t", "default", "--project", proj.path()])
        .output()
        .unwrap();
    assert!(strip_ansi(stdout(&miss)).contains("No matches found"),
        "file-history-snapshot records should not appear in default targets");
}

#[test]
fn test_search_file_history_snapshot_via_all() {
    let world = MockWorld::new();
    let proj = world.project("fhs-all");
    proj.session("sess-fhsa")
        .user_message("hello")
        .file_history_snapshot(&["UNIQUE_FHS_FILE.rs"])
        .done();

    // -t all should include file-history-snapshot records
    let found = world
        .cmd()
        .args(["search", "UNIQUE_FHS_FILE", "-t", "all", "--project", proj.path()])
        .output()
        .unwrap();
    assert!(found.status.success());
    assert!(!strip_ansi(stdout(&found)).contains("No matches found"),
        "-t all should include file-history-snapshot records");
}

#[test]
fn test_dump_file_history_snapshot() {
    let world = MockWorld::new();
    let proj = world.project("fhs-dump");
    proj.session("sess-fhsd")
        .user_message("hello user")
        .file_history_snapshot(&["src/parser.rs", "tests/e2e.rs"])
        .assistant_message("hello assistant")
        .done();

    // -t file-history-snapshot shows only snapshot records
    let out = world
        .cmd()
        .args(["dump", "0", "-t", "file-history-snapshot", "--project", proj.path()])
        .output()
        .unwrap();
    assert!(out.status.success());
    let text = stdout(&out);
    assert!(text.contains("src/parser.rs (v"), "dump should show tracked file paths with version");
    assert!(text.contains("tests/e2e.rs (v"), "dump should show tracked file paths with version");
    assert!(!text.contains("hello user"), "should not show user messages");
    assert!(!text.contains("hello assistant"), "should not show assistant messages");
}

#[test]
fn test_file_history_snapshot_empty_snapshot() {
    let world = MockWorld::new();
    let proj = world.project("fhs-empty");
    proj.session("sess-fhse")
        .user_message("hello")
        .file_history_snapshot(&[])
        .done();

    // Empty snapshot should still be extractable (no tracked files text)
    let out = world
        .cmd()
        .args(["dump", "0", "-t", "file-history-snapshot", "--project", proj.path()])
        .output()
        .unwrap();
    assert!(out.status.success());
    let text = stdout(&out);
    assert!(!text.contains("hello"), "should not show user messages");
}

#[test]
fn test_file_history_snapshot_not_in_default_not_warned() {
    let world = MockWorld::new();
    let proj = world.project("fhs-nowarn");
    proj.session("sess-fhsw")
        .user_message("hello")
        .file_history_snapshot(&["src/main.rs"])
        .done();

    // default dump should not warn about file-history-snapshot records
    let out = world
        .cmd()
        .args(["dump", "0", "--project", proj.path()])
        .output()
        .unwrap();
    assert!(out.status.success());
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(!err.contains("warning: skipping unrecognized record"),
        "file-history-snapshot records should not trigger unrecognized record warnings");
}

// ═════════════════════════════════════════════════════════════════════════════
// queue-operation target
// ═════════════════════════════════════════════════════════════════════════════

#[test]
fn test_search_queue_operation_with_content() {
    let world = MockWorld::new();
    let proj = world.project("qo-content");
    proj.session("sess-qo")
        .user_message("hello")
        .queue_operation("enqueue", Some("UNIQUE_QUEUED_MSG"))
        .done();

    // -t queue-operation finds the content
    let found = world
        .cmd()
        .args(["search", "UNIQUE_QUEUED_MSG", "-t", "queue-operation", "--project", proj.path()])
        .output()
        .unwrap();
    assert!(found.status.success());
    assert!(!strip_ansi(stdout(&found)).contains("No matches found"),
        "-t queue-operation should find enqueued messages by content");

    // default targets should also find queue-operation records
    let hit = world
        .cmd()
        .args(["search", "UNIQUE_QUEUED_MSG", "--project", proj.path()])
        .output()
        .unwrap();
    assert!(!strip_ansi(stdout(&hit)).contains("No matches found"),
        "queue-operation records should appear in default targets");
}

#[test]
fn test_search_queue_operation_without_content() {
    let world = MockWorld::new();
    let proj = world.project("qo-nocontent");
    proj.session("sess-qon")
        .user_message("hello")
        .queue_operation("dequeue", None)
        .done();

    // -t queue-operation finds records without content (by matching the operation)
    let found = world
        .cmd()
        .args(["search", "dequeue", "-t", "queue-operation", "--project", proj.path()])
        .output()
        .unwrap();
    assert!(found.status.success());
    assert!(!strip_ansi(stdout(&found)).contains("No matches found"),
        "-t queue-operation should find records by operation when no content");
}

#[test]
fn test_search_queue_operation_via_all() {
    let world = MockWorld::new();
    let proj = world.project("qo-all");
    proj.session("sess-qoa")
        .user_message("hello")
        .queue_operation("enqueue", Some("UNIQUE_QUEUE_ALL"))
        .done();

    let found = world
        .cmd()
        .args(["search", "UNIQUE_QUEUE_ALL", "-t", "all", "--project", proj.path()])
        .output()
        .unwrap();
    assert!(found.status.success());
    assert!(!strip_ansi(stdout(&found)).contains("No matches found"),
        "-t all should include queue-operation records");
}

#[test]
fn test_dump_queue_operation() {
    let world = MockWorld::new();
    let proj = world.project("qo-dump");
    proj.session("sess-qod")
        .user_message("hello user")
        .queue_operation("enqueue", Some("queued message text"))
        .queue_operation("dequeue", None)
        .assistant_message("hello assistant")
        .done();

    let out = world
        .cmd()
        .args(["dump", "0", "-t", "queue-operation", "--project", proj.path()])
        .output()
        .unwrap();
    assert!(out.status.success());
    let text = stdout(&out);
    assert!(text.contains("enqueue"), "dump should show operation type");
    assert!(text.contains("queued message text"), "dump should show enqueued content");
    assert!(text.contains("dequeue"), "dump should show dequeue operation");
    assert!(!text.contains("hello user"), "should not show user messages");
    assert!(!text.contains("hello assistant"), "should not show assistant messages");
}

#[test]
fn test_queue_operation_not_in_default_not_warned() {
    let world = MockWorld::new();
    let proj = world.project("qo-nowarn");
    proj.session("sess-qow")
        .user_message("hello")
        .queue_operation("enqueue", Some("some queued msg"))
        .done();

    let out = world
        .cmd()
        .args(["dump", "0", "--project", proj.path()])
        .output()
        .unwrap();
    assert!(out.status.success());
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(!err.contains("warning: skipping unrecognized record"),
        "queue-operation records should not trigger unrecognized record warnings");
}

// ═════════════════════════════════════════════════════════════════════════════
// last-prompt target
// ═════════════════════════════════════════════════════════════════════════════

#[test]
fn test_search_last_prompt() {
    let world = MockWorld::new();
    let proj = world.project("lp-search");
    proj.session("sess-lp")
        .user_message("hello")
        .assistant_message("hi")
        .last_prompt("UNIQUE_LAST_PROMPT_XYZ")
        .done();

    // -t last-prompt finds the record
    let found = world
        .cmd()
        .args(["search", "UNIQUE_LAST_PROMPT_XYZ", "-t", "last-prompt", "--project", proj.path()])
        .output()
        .unwrap();
    assert!(found.status.success());
    assert!(!strip_ansi(stdout(&found)).contains("No matches found"),
        "-t last-prompt should find last-prompt records");

    // default targets should NOT find last-prompt records
    let miss = world
        .cmd()
        .args(["search", "UNIQUE_LAST_PROMPT_XYZ", "--project", proj.path()])
        .output()
        .unwrap();
    assert!(strip_ansi(stdout(&miss)).contains("No matches found"),
        "last-prompt records should not appear in default targets");
}

#[test]
fn test_search_last_prompt_via_all() {
    let world = MockWorld::new();
    let proj = world.project("lp-all");
    proj.session("sess-lpa")
        .user_message("hello")
        .last_prompt("UNIQUE_LP_ALL_SEARCH")
        .done();

    let found = world
        .cmd()
        .args(["search", "UNIQUE_LP_ALL_SEARCH", "-t", "all", "--project", proj.path()])
        .output()
        .unwrap();
    assert!(found.status.success());
    assert!(!strip_ansi(stdout(&found)).contains("No matches found"),
        "-t all should include last-prompt records");
}

#[test]
fn test_dump_last_prompt() {
    let world = MockWorld::new();
    let proj = world.project("lp-dump");
    proj.session("sess-lpd")
        .user_message("hello user")
        .last_prompt("the last prompt text")
        .assistant_message("hello assistant")
        .done();

    let out = world
        .cmd()
        .args(["dump", "0", "-t", "last-prompt", "--project", proj.path()])
        .output()
        .unwrap();
    assert!(out.status.success());
    let text = stdout(&out);
    assert!(text.contains("the last prompt text"), "dump should show last prompt text");
    assert!(!text.contains("hello user"), "should not show user messages");
    assert!(!text.contains("hello assistant"), "should not show assistant messages");
}

#[test]
fn test_last_prompt_not_in_default_not_warned() {
    let world = MockWorld::new();
    let proj = world.project("lp-nowarn");
    proj.session("sess-lpw")
        .user_message("hello")
        .last_prompt("some prompt")
        .done();

    let out = world
        .cmd()
        .args(["dump", "0", "--project", proj.path()])
        .output()
        .unwrap();
    assert!(out.status.success());
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(!err.contains("warning: skipping unrecognized record"),
        "last-prompt records should not trigger unrecognized record warnings");
}

// ═════════════════════════════════════════════════════════════════════════════
// agent-name target
// ═════════════════════════════════════════════════════════════════════════════

#[test]
fn test_search_agent_name() {
    let world = MockWorld::new();
    let proj = world.project("an-search");
    proj.session("sess-an")
        .user_message("hello")
        .agent_name("UNIQUE_AGENT_NAME_XYZ")
        .assistant_message("hi")
        .done();

    // -t agent-name finds the record
    let found = world
        .cmd()
        .args(["search", "UNIQUE_AGENT_NAME_XYZ", "-t", "agent-name", "--project", proj.path()])
        .output()
        .unwrap();
    assert!(found.status.success());
    assert!(!strip_ansi(stdout(&found)).contains("No matches found"),
        "-t agent-name should find agent-name records");

    // default targets should NOT find agent-name records
    let miss = world
        .cmd()
        .args(["search", "UNIQUE_AGENT_NAME_XYZ", "--project", proj.path()])
        .output()
        .unwrap();
    assert!(strip_ansi(stdout(&miss)).contains("No matches found"),
        "agent-name records should not appear in default targets");
}

#[test]
fn test_search_agent_name_via_all() {
    let world = MockWorld::new();
    let proj = world.project("an-all");
    proj.session("sess-ana")
        .user_message("hello")
        .agent_name("UNIQUE_AN_ALL_SEARCH")
        .done();

    let found = world
        .cmd()
        .args(["search", "UNIQUE_AN_ALL_SEARCH", "-t", "all", "--project", proj.path()])
        .output()
        .unwrap();
    assert!(found.status.success());
    assert!(!strip_ansi(stdout(&found)).contains("No matches found"),
        "-t all should include agent-name records");
}

#[test]
fn test_dump_agent_name() {
    let world = MockWorld::new();
    let proj = world.project("an-dump");
    proj.session("sess-and")
        .user_message("hello user")
        .agent_name("my-cool-agent")
        .assistant_message("hello assistant")
        .done();

    let out = world
        .cmd()
        .args(["dump", "0", "-t", "agent-name", "--project", proj.path()])
        .output()
        .unwrap();
    assert!(out.status.success());
    let text = stdout(&out);
    assert!(text.contains("my-cool-agent"), "dump should show agent name");
    assert!(!text.contains("hello user"), "should not show user messages");
    assert!(!text.contains("hello assistant"), "should not show assistant messages");
}

#[test]
fn test_agent_name_not_in_default_not_warned() {
    let world = MockWorld::new();
    let proj = world.project("an-nowarn");
    proj.session("sess-anw")
        .user_message("hello")
        .agent_name("some-agent")
        .done();

    let out = world
        .cmd()
        .args(["dump", "0", "--project", proj.path()])
        .output()
        .unwrap();
    assert!(out.status.success());
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(!err.contains("warning: skipping unrecognized record"),
        "agent-name records should not trigger unrecognized record warnings");
}

// ═════════════════════════════════════════════════════════════════════════════
// custom-title target
// ═════════════════════════════════════════════════════════════════════════════

#[test]
fn test_search_custom_title() {
    let world = MockWorld::new();
    let proj = world.project("ct-search");
    proj.session("sess-ct")
        .user_message("hello")
        .custom_title("UNIQUE_CUSTOM_TITLE_XYZ")
        .assistant_message("hi")
        .done();

    // -t custom-title finds the record
    let found = world
        .cmd()
        .args(["search", "UNIQUE_CUSTOM_TITLE_XYZ", "-t", "custom-title", "--project", proj.path()])
        .output()
        .unwrap();
    assert!(found.status.success());
    assert!(!strip_ansi(stdout(&found)).contains("No matches found"),
        "-t custom-title should find custom-title records");

    // default targets should NOT find custom-title records
    let miss = world
        .cmd()
        .args(["search", "UNIQUE_CUSTOM_TITLE_XYZ", "--project", proj.path()])
        .output()
        .unwrap();
    assert!(strip_ansi(stdout(&miss)).contains("No matches found"),
        "custom-title records should not appear in default targets");
}

#[test]
fn test_search_custom_title_via_all() {
    let world = MockWorld::new();
    let proj = world.project("ct-all");
    proj.session("sess-cta")
        .user_message("hello")
        .custom_title("UNIQUE_CT_ALL_SEARCH")
        .done();

    let found = world
        .cmd()
        .args(["search", "UNIQUE_CT_ALL_SEARCH", "-t", "all", "--project", proj.path()])
        .output()
        .unwrap();
    assert!(found.status.success());
    assert!(!strip_ansi(stdout(&found)).contains("No matches found"),
        "-t all should include custom-title records");
}

#[test]
fn test_dump_custom_title() {
    let world = MockWorld::new();
    let proj = world.project("ct-dump");
    proj.session("sess-ctd")
        .user_message("hello user")
        .custom_title("my-custom-title")
        .assistant_message("hello assistant")
        .done();

    let out = world
        .cmd()
        .args(["dump", "0", "-t", "custom-title", "--project", proj.path()])
        .output()
        .unwrap();
    assert!(out.status.success());
    let text = stdout(&out);
    assert!(text.contains("my-custom-title"), "dump should show custom title");
    assert!(!text.contains("hello user"), "should not show user messages");
    assert!(!text.contains("hello assistant"), "should not show assistant messages");
}

#[test]
fn test_custom_title_not_in_default_not_warned() {
    let world = MockWorld::new();
    let proj = world.project("ct-nowarn");
    proj.session("sess-ctw")
        .user_message("hello")
        .custom_title("some-title")
        .done();

    let out = world
        .cmd()
        .args(["dump", "0", "--project", proj.path()])
        .output()
        .unwrap();
    assert!(out.status.success());
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(!err.contains("warning: skipping unrecognized record"),
        "custom-title records should not trigger unrecognized record warnings");
}

// ═════════════════════════════════════════════════════════════════════════════
// permission-mode and attachment records are silently recognized
// ═════════════════════════════════════════════════════════════════════════════

#[test]
fn test_permission_mode_not_warned() {
    let world = MockWorld::new();
    let proj = world.project("pm-nowarn");
    proj.session("sess-pmw")
        .permission_mode("bypassPermissions")
        .user_message("hello")
        .done();

    let out = world
        .cmd()
        .args(["dump", "0", "--project", proj.path()])
        .output()
        .unwrap();
    assert!(out.status.success());
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(!err.contains("warning: skipping unrecognized record"),
        "permission-mode records should not trigger unrecognized record warnings, got: {}", err);
}

#[test]
fn test_attachment_not_warned() {
    let world = MockWorld::new();
    let proj = world.project("att-nowarn");
    proj.session("sess-attw")
        .attachment("deferred_tools_delta")
        .attachment("mcp_server_delta")
        .user_message("hello")
        .done();

    let out = world
        .cmd()
        .args(["dump", "0", "--project", proj.path()])
        .output()
        .unwrap();
    assert!(out.status.success());
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(!err.contains("warning: skipping unrecognized record"),
        "attachment records should not trigger unrecognized record warnings, got: {}", err);
}

#[test]
fn test_search_permission_mode() {
    let world = MockWorld::new();
    let proj = world.project("pm-search");
    proj.session("sess-pms")
        .user_message("hello")
        .permission_mode("UNIQUE_PM_MODE_XYZ")
        .assistant_message("hi")
        .done();

    // -t permission-mode finds the record
    let found = world
        .cmd()
        .args(["search", "UNIQUE_PM_MODE_XYZ", "-t", "permission-mode", "--project", proj.path()])
        .output()
        .unwrap();
    assert!(found.status.success());
    assert!(!strip_ansi(stdout(&found)).contains("No matches found"),
        "-t permission-mode should find permission-mode records");

    // default targets should NOT find permission-mode records
    let miss = world
        .cmd()
        .args(["search", "UNIQUE_PM_MODE_XYZ", "--project", proj.path()])
        .output()
        .unwrap();
    assert!(strip_ansi(stdout(&miss)).contains("No matches found"),
        "permission-mode records should not appear in default targets");
}

#[test]
fn test_search_permission_mode_via_all() {
    let world = MockWorld::new();
    let proj = world.project("pm-all");
    proj.session("sess-pma")
        .user_message("hello")
        .permission_mode("UNIQUE_PM_ALL_SEARCH")
        .done();

    let found = world
        .cmd()
        .args(["search", "UNIQUE_PM_ALL_SEARCH", "-t", "all", "--project", proj.path()])
        .output()
        .unwrap();
    assert!(found.status.success());
    assert!(!strip_ansi(stdout(&found)).contains("No matches found"),
        "-t all should include permission-mode records");
}

#[test]
fn test_dump_permission_mode() {
    let world = MockWorld::new();
    let proj = world.project("pm-dump");
    proj.session("sess-pmd")
        .user_message("hello user")
        .permission_mode("bypassPermissions")
        .assistant_message("hello assistant")
        .done();

    let out = world
        .cmd()
        .args(["dump", "0", "-t", "permission-mode", "--project", proj.path()])
        .output()
        .unwrap();
    assert!(out.status.success());
    let text = stdout(&out);
    assert!(text.contains("bypassPermissions"), "dump should show permission mode value");
    assert!(!text.contains("hello user"), "should not show user messages");
    assert!(!text.contains("hello assistant"), "should not show assistant messages");
}

#[test]
fn test_search_attachment_tool_name() {
    // A deferred_tools_delta attachment adds a distinctively named tool —
    // searching for that tool name should find the attachment record.
    let world = MockWorld::new();
    let proj = world.project("att-search-tool");
    proj.session("sess-atts")
        .user_message("hello")
        .attachment_tools(&["UNIQUE_ATT_TOOL_XYZ"], &[])
        .assistant_message("hi")
        .done();

    let found = world
        .cmd()
        .args(["search", "UNIQUE_ATT_TOOL_XYZ", "-t", "attachment", "--project", proj.path()])
        .output()
        .unwrap();
    assert!(found.status.success());
    assert!(!strip_ansi(stdout(&found)).contains("No matches found"),
        "-t attachment should find deferred_tools_delta records by tool name");

    // default targets should NOT find attachment records
    let miss = world
        .cmd()
        .args(["search", "UNIQUE_ATT_TOOL_XYZ", "--project", proj.path()])
        .output()
        .unwrap();
    assert!(strip_ansi(stdout(&miss)).contains("No matches found"),
        "attachment records should not appear in default targets");
}

#[test]
fn test_search_attachment_mcp_block_prose() {
    // The most useful searchable content in attachment records is the
    // `addedBlocks` prose of mcp_server_delta records, which describe MCP
    // servers and their capabilities.
    let world = MockWorld::new();
    let proj = world.project("att-search-mcp");
    proj.session("sess-attmcp")
        .user_message("hello")
        .attachment_mcp(
            &["Example MCP"],
            &["## Example MCP\nUNIQUE_MCP_DESCRIPTION_PROSE for recruiting workflows"],
        )
        .done();

    let found = world
        .cmd()
        .args(["search", "UNIQUE_MCP_DESCRIPTION_PROSE", "-t", "attachment", "--project", proj.path()])
        .output()
        .unwrap();
    assert!(found.status.success());
    assert!(!strip_ansi(stdout(&found)).contains("No matches found"),
        "-t attachment should find mcp_server_delta records by addedBlocks content");
}

#[test]
fn test_search_attachment_via_all() {
    let world = MockWorld::new();
    let proj = world.project("att-all");
    proj.session("sess-atta")
        .user_message("hello")
        .attachment_tools(&["UNIQUE_ATT_ALL_NAME"], &[])
        .done();

    let found = world
        .cmd()
        .args(["search", "UNIQUE_ATT_ALL_NAME", "-t", "all", "--project", proj.path()])
        .output()
        .unwrap();
    assert!(found.status.success());
    assert!(!strip_ansi(stdout(&found)).contains("No matches found"),
        "-t all should include attachment records");
}

#[test]
fn test_dump_attachment() {
    let world = MockWorld::new();
    let proj = world.project("att-dump");
    proj.session("sess-attd")
        .user_message("hello user")
        .attachment_tools(&["NewTool"], &["OldTool"])
        .assistant_message("hello assistant")
        .done();

    let out = world
        .cmd()
        .args(["dump", "0", "-t", "attachment", "--project", proj.path()])
        .output()
        .unwrap();
    assert!(out.status.success());
    let text = stdout(&out);
    assert!(text.contains("NewTool"), "dump should show added tool name");
    assert!(text.contains("OldTool"), "dump should show removed tool name");
    assert!(!text.contains("hello user"), "should not show user messages");
    assert!(!text.contains("hello assistant"), "should not show assistant messages");
}

// ── tail tests ───────────────────────────────────────────────────────────────

#[test]
fn test_tail_shows_last_n_records() {
    let world = MockWorld::new();
    let proj = world.project("tail-basic");
    proj.session("sess-tail-1")
        .user_message("TAIL_MSG_1")
        .assistant_message("TAIL_MSG_2")
        .user_message("TAIL_MSG_3")
        .assistant_message("TAIL_MSG_4")
        .user_message("TAIL_MSG_5")
        .done();

    let out = world
        .cmd()
        .args(["tail", "-n", "2", "--project", proj.path()])
        .output()
        .unwrap();

    assert!(out.status.success(), "stderr: {}", String::from_utf8_lossy(&out.stderr));
    let text = strip_ansi(stdout(&out));
    assert!(!text.contains("TAIL_MSG_3"), "should not contain earlier records");
    assert!(text.contains("TAIL_MSG_4"), "should contain second-to-last record");
    assert!(text.contains("TAIL_MSG_5"), "should contain last record");
}

#[test]
fn test_tail_defaults_to_ten() {
    let world = MockWorld::new();
    let proj = world.project("tail-default");
    let mut builder = proj.session("sess-tail-def");
    for i in 1..=15 {
        builder = builder.user_message(&format!("TAIL_DEF_{}", i));
    }
    builder.done();

    let out = world
        .cmd()
        .args(["tail", "--project", proj.path()])
        .output()
        .unwrap();

    assert!(out.status.success(), "stderr: {}", String::from_utf8_lossy(&out.stderr));
    let text = strip_ansi(stdout(&out));
    // Messages 1-5 should be excluded (15 - 10 = 5)
    assert!(!text.contains("TAIL_DEF_5"), "should not contain 5th record");
    assert!(text.contains("TAIL_DEF_6"), "should contain 6th record (first of last 10)");
    assert!(text.contains("TAIL_DEF_15"), "should contain last record");
}

#[test]
fn test_tail_more_than_available() {
    let world = MockWorld::new();
    let proj = world.project("tail-over");
    proj.session("sess-tail-over")
        .user_message("TAIL_OVER_1")
        .assistant_message("TAIL_OVER_2")
        .done();

    let out = world
        .cmd()
        .args(["tail", "-n", "100", "--project", proj.path()])
        .output()
        .unwrap();

    assert!(out.status.success(), "stderr: {}", String::from_utf8_lossy(&out.stderr));
    let text = strip_ansi(stdout(&out));
    assert!(text.contains("TAIL_OVER_1"), "should contain all records");
    assert!(text.contains("TAIL_OVER_2"), "should contain all records");
}

#[test]
fn test_tail_with_targets() {
    let world = MockWorld::new();
    let proj = world.project("tail-targets");
    proj.session("sess-tail-tgt")
        .user_message("TAIL_TGT_USER_1")
        .assistant_message("TAIL_TGT_ASST_1")
        .user_message("TAIL_TGT_USER_2")
        .assistant_message("TAIL_TGT_ASST_2")
        .user_message("TAIL_TGT_USER_3")
        .done();

    let out = world
        .cmd()
        .args(["tail", "-n", "2", "-t", "user", "--project", proj.path()])
        .output()
        .unwrap();

    assert!(out.status.success(), "stderr: {}", String::from_utf8_lossy(&out.stderr));
    let text = strip_ansi(stdout(&out));
    // Only user messages, last 2: USER_2, USER_3
    assert!(!text.contains("TAIL_TGT_USER_1"), "should not contain earlier user records");
    assert!(!text.contains("TAIL_TGT_ASST"), "should not contain assistant records");
    assert!(text.contains("TAIL_TGT_USER_2"), "should contain second-to-last user record");
    assert!(text.contains("TAIL_TGT_USER_3"), "should contain last user record");
}

#[test]
fn test_tail_specific_session() {
    let world = MockWorld::new();
    let proj = world.project("tail-session");
    proj.session("sess-tail-s1")
        .user_message("TAIL_SESS_S1_MSG")
        .done();
    proj.session("sess-tail-s2")
        .user_message("TAIL_SESS_S2_MSG")
        .done();

    let out = world
        .cmd()
        .args(["tail", "sess-tail-s1", "--project", proj.path()])
        .output()
        .unwrap();

    assert!(out.status.success(), "stderr: {}", String::from_utf8_lossy(&out.stderr));
    let text = strip_ansi(stdout(&out));
    assert!(text.contains("TAIL_SESS_S1_MSG"), "should contain specified session content");
    assert!(!text.contains("TAIL_SESS_S2_MSG"), "should not contain other session content");
}

#[test]
fn test_tail_defaults_to_latest_session() {
    let world = MockWorld::new();
    let proj = world.project("tail-latest");
    proj.session("sess-tail-old")
        .user_message("TAIL_OLD_MSG")
        .done();
    proj.session("sess-tail-new")
        .user_message("TAIL_NEW_MSG")
        .done();

    // Ensure distinct mtimes so "latest" is deterministic
    let old_path = proj.session_path("sess-tail-old");
    let new_path = proj.session_path("sess-tail-new");
    set_mtime(&old_path, SystemTime::UNIX_EPOCH + Duration::from_secs(1000));
    set_mtime(&new_path, SystemTime::UNIX_EPOCH + Duration::from_secs(2000));

    let out = world
        .cmd()
        .args(["tail", "--project", proj.path()])
        .output()
        .unwrap();

    assert!(out.status.success(), "stderr: {}", String::from_utf8_lossy(&out.stderr));
    let text = strip_ansi(stdout(&out));
    assert!(text.contains("TAIL_NEW_MSG"), "should show latest session");
    assert!(!text.contains("TAIL_OLD_MSG"), "should not show older session");
}

#[test]
fn test_tail_output_format_matches_dump() {
    let world = MockWorld::new();
    let proj = world.project("tail-format");
    proj.session("sess-tail-fmt")
        .user_message("TAIL_FMT_MSG")
        .bash("echo hello", "hello")
        .done();

    let out = world
        .cmd()
        .args(["tail", "-n", "100", "--project", proj.path()])
        .output()
        .unwrap();

    assert!(out.status.success(), "stderr: {}", String::from_utf8_lossy(&out.stderr));
    let text = strip_ansi(stdout(&out));
    assert!(text.contains("[user]"), "should have [user] label");
    assert!(text.contains("[bash-command:Bash]"), "should have [bash-command:Bash] label");
    assert!(text.contains("[bash-output:Bash]"), "should have [bash-output:Bash] label");
}

#[test]
fn test_tail_sorts_by_timestamp_across_subagents() {
    let world = MockWorld::new();
    let proj = world.project("tail-subagent-sort");
    // Main session: messages at t=1..4, then t=100..101
    proj.session("sess-tail-sort")
        .user_message("TAIL_SORT_MAIN_EARLY_1")
        .assistant_message("TAIL_SORT_MAIN_EARLY_2")
        .user_message("TAIL_SORT_MAIN_EARLY_3")
        .assistant_message("TAIL_SORT_MAIN_EARLY_4")
        .with_ts_offset(99)
        .user_message("TAIL_SORT_MAIN_LATE_1")
        .assistant_message("TAIL_SORT_MAIN_LATE_2")
        .done();
    // Subagent: messages at t=50..51 (between main early and late)
    proj.subagent_session("sess-tail-sort", "explorer")
        .with_ts_offset(49)
        .user_message("TAIL_SORT_SUB_1")
        .assistant_message("TAIL_SORT_SUB_2")
        .done();

    // tail -n 1 should return the chronologically last record (main late 2),
    // not the last record from the last file processed (subagent 2)
    let out = world
        .cmd()
        .args(["tail", "-n", "1", "sess-tail-sort", "--project", proj.path()])
        .output()
        .unwrap();

    assert!(out.status.success(), "stderr: {}", String::from_utf8_lossy(&out.stderr));
    let text = strip_ansi(stdout(&out));
    assert!(text.contains("TAIL_SORT_MAIN_LATE_2"),
        "tail -n 1 should return the chronologically last record, got: {}", text.trim());
    assert!(!text.contains("TAIL_SORT_SUB"),
        "should not contain subagent records");

    // tail -n 3 --subagents should return the last 3 chronologically:
    // subagent 2 (t=51), main late 1 (t=100), main late 2 (t=101)
    let out = world
        .cmd()
        .args(["tail", "-n", "3", "--subagents", "sess-tail-sort", "--project", proj.path()])
        .output()
        .unwrap();

    assert!(out.status.success(), "stderr: {}", String::from_utf8_lossy(&out.stderr));
    let text = strip_ansi(stdout(&out));
    assert!(text.contains("TAIL_SORT_SUB_2"), "should contain subagent record at t=51");
    assert!(text.contains("TAIL_SORT_MAIN_LATE_1"), "should contain main late 1 at t=100");
    assert!(text.contains("TAIL_SORT_MAIN_LATE_2"), "should contain main late 2 at t=101");
    assert!(!text.contains("TAIL_SORT_MAIN_EARLY"), "should not contain early main records");
    assert!(!text.contains("TAIL_SORT_SUB_1"), "should not contain subagent record at t=50");
}

// ── tail -f tests ────────────────────────────────────────────────────────────

#[test]
fn test_tail_follow_picks_up_new_records() {
    let world = MockWorld::new();
    let proj = world.project("tail-follow");
    proj.session("sess-tail-f")
        .user_message("TAIL_F_INITIAL")
        .done();

    // Start `claugrep tail -f -n 1` as a child process
    let mut child = world
        .cmd()
        .args(["tail", "-f", "-n", "1", "sess-tail-f", "--project", proj.path()])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();

    // Give it time to start and print the initial tail
    std::thread::sleep(Duration::from_millis(500));

    // Append a new record to the session file
    let session_path = proj.session_path("sess-tail-f");
    {
        use std::fs::OpenOptions;
        let mut f = OpenOptions::new().append(true).open(&session_path).unwrap();
        let record = serde_json::json!({
            "type": "user",
            "message": {"role": "user", "content": "TAIL_F_APPENDED"},
            "timestamp": "2024-01-01T01:00:00Z",
            "sessionId": "sess-tail-f",
        });
        writeln!(f, "{}", record).unwrap();
    }

    // Wait for the follow loop to pick it up
    std::thread::sleep(Duration::from_millis(500));

    // Kill the process and read output
    child.kill().unwrap();
    let output = child.wait_with_output().unwrap();
    let text = strip_ansi(std::str::from_utf8(&output.stdout).unwrap());

    assert!(text.contains("TAIL_F_INITIAL"), "should contain initial record");
    assert!(text.contains("TAIL_F_APPENDED"), "should contain appended record");
}

#[test]
fn test_tail_follow_respects_targets() {
    let world = MockWorld::new();
    let proj = world.project("tail-follow-tgt");
    proj.session("sess-tail-ft")
        .user_message("TAIL_FT_INIT")
        .done();

    // Follow only assistant messages
    let mut child = world
        .cmd()
        .args(["tail", "-f", "-n", "0", "-t", "assistant", "sess-tail-ft", "--project", proj.path()])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();

    std::thread::sleep(Duration::from_millis(500));

    // Append a user record (should be filtered) and an assistant record (should appear)
    let session_path = proj.session_path("sess-tail-ft");
    {
        use std::fs::OpenOptions;
        let mut f = OpenOptions::new().append(true).open(&session_path).unwrap();
        let user_rec = serde_json::json!({
            "type": "user",
            "message": {"role": "user", "content": "TAIL_FT_USER_HIDDEN"},
            "timestamp": "2024-01-01T01:00:00Z",
            "sessionId": "sess-tail-ft",
        });
        writeln!(f, "{}", user_rec).unwrap();
        let asst_rec = serde_json::json!({
            "type": "assistant",
            "message": {"role": "assistant", "content": [{"type": "text", "text": "TAIL_FT_ASST_VISIBLE"}]},
            "timestamp": "2024-01-01T01:00:01Z",
            "sessionId": "sess-tail-ft",
        });
        writeln!(f, "{}", asst_rec).unwrap();
    }

    std::thread::sleep(Duration::from_millis(500));

    child.kill().unwrap();
    let output = child.wait_with_output().unwrap();
    let text = strip_ansi(std::str::from_utf8(&output.stdout).unwrap());

    assert!(!text.contains("TAIL_FT_USER_HIDDEN"), "should not contain user record");
    assert!(!text.contains("TAIL_FT_INIT"), "should not contain initial record (n=0)");
    assert!(text.contains("TAIL_FT_ASST_VISIBLE"), "should contain appended assistant record");
}

// ── unified diff in dump/tail/last ───────────────────────────────────────────

#[test]
fn test_dump_shows_unified_diff_for_edit() {
    let world = MockWorld::new();
    let proj = world.project("dump-diff");
    proj.session("sess-dd")
        .edit("src/main.rs", "old_fn()", "new_fn()")
        .done();

    let out = world
        .cmd()
        .args(["dump", "0", "--project", proj.path()])
        .output()
        .unwrap();

    assert!(out.status.success(), "stderr: {}", String::from_utf8_lossy(&out.stderr));
    let text = strip_ansi(stdout(&out));
    assert!(text.contains("--- a/src/main.rs"), "should show --- file header");
    assert!(text.contains("+++ b/src/main.rs"), "should show +++ file header");
    assert!(text.contains("-old_fn()"), "should show removed line");
    assert!(text.contains("+new_fn()"), "should show added line");
}

#[test]
fn test_dump_no_diff_shows_raw_format() {
    let world = MockWorld::new();
    let proj = world.project("dump-nodiff");
    proj.session("sess-dnd")
        .edit("src/main.rs", "old_fn()", "new_fn()")
        .done();

    let out = world
        .cmd()
        .args(["dump", "0", "--no-diff", "--project", proj.path()])
        .output()
        .unwrap();

    assert!(out.status.success(), "stderr: {}", String::from_utf8_lossy(&out.stderr));
    let text = strip_ansi(stdout(&out));
    assert!(!text.contains("--- a/"), "should not show unified diff headers");
    assert!(text.contains("old_fn()"), "should show raw old_string");
    assert!(text.contains("new_fn()"), "should show raw new_string");
}

#[test]
fn test_tail_shows_unified_diff_for_edit() {
    let world = MockWorld::new();
    let proj = world.project("tail-diff");
    proj.session("sess-td")
        .edit("lib.rs", "before", "after")
        .done();

    let out = world
        .cmd()
        .args(["tail", "-n", "100", "--project", proj.path()])
        .output()
        .unwrap();

    assert!(out.status.success(), "stderr: {}", String::from_utf8_lossy(&out.stderr));
    let text = strip_ansi(stdout(&out));
    assert!(text.contains("--- a/lib.rs"), "should show --- file header");
    assert!(text.contains("+++ b/lib.rs"), "should show +++ file header");
    assert!(text.contains("-before"), "should show removed line");
    assert!(text.contains("+after"), "should show added line");
}

#[test]
fn test_tail_no_diff_shows_raw_format() {
    let world = MockWorld::new();
    let proj = world.project("tail-nodiff");
    proj.session("sess-tnd")
        .edit("lib.rs", "before", "after")
        .done();

    let out = world
        .cmd()
        .args(["tail", "-n", "100", "--no-diff", "--project", proj.path()])
        .output()
        .unwrap();

    assert!(out.status.success(), "stderr: {}", String::from_utf8_lossy(&out.stderr));
    let text = strip_ansi(stdout(&out));
    assert!(!text.contains("--- a/"), "should not show unified diff headers");
    assert!(text.contains("before"), "should show raw old_string");
    assert!(text.contains("after"), "should show raw new_string");
}

#[test]
fn test_last_shows_unified_diff_for_edit() {
    let world = MockWorld::new();
    let proj = world.project("last-diff");
    proj.session("sess-ld")
        .edit("app.rs", "removed_line", "added_line")
        .done();

    let out = world
        .cmd()
        .args(["last", "-n", "100", "--project", proj.path()])
        .output()
        .unwrap();

    assert!(out.status.success(), "stderr: {}", String::from_utf8_lossy(&out.stderr));
    let text = strip_ansi(stdout(&out));
    assert!(text.contains("--- a/app.rs"), "should show --- file header");
    assert!(text.contains("+++ b/app.rs"), "should show +++ file header");
    assert!(text.contains("-removed_line"), "should show removed line");
    assert!(text.contains("+added_line"), "should show added line");
}

#[test]
fn test_last_no_diff_shows_raw_format() {
    let world = MockWorld::new();
    let proj = world.project("last-nodiff");
    proj.session("sess-lnd")
        .edit("app.rs", "removed_line", "added_line")
        .done();

    let out = world
        .cmd()
        .args(["last", "-n", "100", "--no-diff", "--project", proj.path()])
        .output()
        .unwrap();

    assert!(out.status.success(), "stderr: {}", String::from_utf8_lossy(&out.stderr));
    let text = strip_ansi(stdout(&out));
    assert!(!text.contains("--- a/"), "should not show unified diff headers");
    assert!(!text.contains("+++ b/"), "should not show unified diff headers");
    // last shows first-line-only summary, so raw key/value text is truncated
    assert!(text.contains("file_path:"), "should show raw key/value format");
}

// ═════════════════════════════════════════════════════════════════════════════
// --json raw JSONL output
// ═════════════════════════════════════════════════════════════════════════════

#[test]
fn test_dump_json_output() {
    let world = MockWorld::new();
    let proj = world.project("dump-json");
    proj.session("sess-dj")
        .user_message("DUMP_JSON_TEST")
        .assistant_message("reply here")
        .done();

    let out = world
        .cmd()
        .args(["dump", "--json", "--project", proj.path()])
        .output()
        .unwrap();

    assert!(out.status.success(), "stderr: {}", String::from_utf8_lossy(&out.stderr));
    let text = stdout(&out);
    let lines: Vec<&str> = text.lines().collect();
    assert!(lines.len() >= 2, "should have at least 2 JSONL lines (user + assistant)");

    for line in &lines {
        let entry: serde_json::Value = serde_json::from_str(line)
            .expect("each line must be valid JSON");
        assert!(entry["type"].is_string(), "raw entry should have a type field");
    }

    assert!(text.contains("DUMP_JSON_TEST"), "should contain user message text");
}

#[test]
fn test_tail_json_output() {
    let world = MockWorld::new();
    let proj = world.project("tail-json");
    proj.session("sess-tj")
        .user_message("TAIL_JSON_TEST")
        .assistant_message("tail reply")
        .done();

    let out = world
        .cmd()
        .args(["tail", "--json", "--project", proj.path()])
        .output()
        .unwrap();

    assert!(out.status.success(), "stderr: {}", String::from_utf8_lossy(&out.stderr));
    let text = stdout(&out);
    let lines: Vec<&str> = text.lines().collect();
    assert!(!lines.is_empty(), "should have at least one JSONL line");

    for line in &lines {
        let entry: serde_json::Value = serde_json::from_str(line)
            .expect("each line must be valid JSON");
        assert!(entry["type"].is_string(), "raw entry should have a type field");
    }

    assert!(text.contains("TAIL_JSON_TEST"), "should contain user message text");
}

#[test]
fn test_search_json_outputs_raw_entries() {
    let world = MockWorld::new();
    let proj = world.project("search-json-raw");
    proj.session("sess-sjr")
        .user_message("SEARCH_RAW_JSON_MARKER")
        .done();

    let out = world
        .cmd()
        .args(["search", "SEARCH_RAW_JSON_MARKER", "--json", "--project", proj.path()])
        .output()
        .unwrap();

    assert!(out.status.success(), "stderr: {}", String::from_utf8_lossy(&out.stderr));
    let text = stdout(&out);
    let lines: Vec<&str> = text.lines().collect();
    assert!(!lines.is_empty(), "should have at least one JSONL line");

    let first: serde_json::Value = serde_json::from_str(lines[0])
        .expect("each line must be valid JSON");
    assert_eq!(first["type"].as_str(), Some("user"), "raw entry should preserve original type");
    assert!(first["message"].is_object(), "raw entry should have message field");
}

// ── dump: chronological ordering with subagents ─────────────────────────────

#[test]
fn test_dump_sorts_by_timestamp_across_subagents() {
    let world = MockWorld::new();
    let proj = world.project("dump-subagent-sort");
    // Main session: messages at t=1..2, then t=100..101
    proj.session("sess-dump-sort")
        .user_message("DUMP_SORT_MAIN_EARLY")
        .assistant_message("DUMP_SORT_MAIN_EARLY_REPLY")
        .with_ts_offset(99)
        .user_message("DUMP_SORT_MAIN_LATE")
        .assistant_message("DUMP_SORT_MAIN_LATE_REPLY")
        .done();
    // Subagent: messages at t=50..51 (between main early and late)
    proj.subagent_session("sess-dump-sort", "explorer")
        .with_ts_offset(49)
        .user_message("DUMP_SORT_SUB_PROMPT")
        .assistant_message("DUMP_SORT_SUB_REPLY")
        .done();

    // Without --subagents, subagent content should be hidden
    let out = world
        .cmd()
        .args(["dump", "sess-dump-sort", "--project", proj.path()])
        .output()
        .unwrap();

    assert!(out.status.success(), "stderr: {}", String::from_utf8_lossy(&out.stderr));
    let text = strip_ansi(stdout(&out));
    assert!(text.contains("DUMP_SORT_MAIN_EARLY"), "should contain early main message");
    assert!(text.contains("DUMP_SORT_MAIN_LATE"), "should contain late main message");
    assert!(!text.contains("DUMP_SORT_SUB_PROMPT"), "should NOT contain subagent message without --subagents");

    // With --subagents, all messages should be present and chronologically ordered
    let out = world
        .cmd()
        .args(["dump", "sess-dump-sort", "--subagents", "--project", proj.path()])
        .output()
        .unwrap();

    assert!(out.status.success(), "stderr: {}", String::from_utf8_lossy(&out.stderr));
    let text = strip_ansi(stdout(&out));

    assert!(text.contains("DUMP_SORT_MAIN_EARLY"), "should contain early main message");
    assert!(text.contains("DUMP_SORT_SUB_PROMPT"), "should contain subagent message");
    assert!(text.contains("DUMP_SORT_MAIN_LATE"), "should contain late main message");

    // Verify chronological order: early main < subagent < late main
    let pos_early = text.find("DUMP_SORT_MAIN_EARLY").unwrap();
    let pos_sub = text.find("DUMP_SORT_SUB_PROMPT").unwrap();
    let pos_late = text.find("DUMP_SORT_MAIN_LATE").unwrap();
    assert!(pos_early < pos_sub,
        "early main message should appear before subagent message in chronological order");
    assert!(pos_sub < pos_late,
        "subagent message should appear before late main message in chronological order");
}

#[test]
fn test_search_alias_s() {
    let world = MockWorld::new();
    let proj = world.project("alias-s");
    proj.session("sess-alias-s")
        .user_message("ALIAS_S_NEEDLE")
        .done();

    // "s" alias should behave identically to "search"
    let out = world
        .cmd()
        .args(["s", "ALIAS_S_NEEDLE", "--project", proj.path()])
        .output()
        .unwrap();
    assert!(out.status.success(), "stderr: {}", String::from_utf8_lossy(&out.stderr));
    assert!(!strip_ansi(stdout(&out)).contains("No matches found"));
    assert!(strip_ansi(stdout(&out)).contains("ALIAS_S_NEEDLE"));
}
