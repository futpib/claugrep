//! Hermetic end-to-end tests using a self-contained mock world.
//!
//! Each test builds synthetic Claude session JSONL files in a temporary HOME
//! directory and invokes the real `claugrep` binary against them.  No real
//! Claude session data is required — every test is fully deterministic.

use std::fs;
use std::io::Write as IoWrite;
use std::path::PathBuf;
use std::process::Command;
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
    let parsed: serde_json::Value = serde_json::from_str(stdout(&out))
        .expect("--json must produce valid JSON");
    let arr = parsed.as_array().expect("expected JSON array");
    assert!(!arr.is_empty());
    let first = &arr[0];
    assert!(first["sessionId"].is_string());
    assert!(first["timestamp"].is_string());
    assert!(first["target"].is_string());
    assert!(first["text"].is_string());
    assert!(first["text"].as_str().unwrap().contains("LAST_JSON_CONTENT"));
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
// search — target flags
// ═════════════════════════════════════════════════════════════════════════════

#[test]
fn test_search_assistant_flag() {
    let world = MockWorld::new();
    let proj = world.project("asst-flag");
    proj.session("sess-af")
        .user_message("USER_ONLY_TEXT_AF")
        .assistant_message("ASST_UNIQUE_TEXT_AF")
        .done();

    // --assistant finds assistant text.
    let found = world
        .cmd()
        .args(["search", "ASST_UNIQUE_TEXT_AF", "--assistant", "--project", proj.path()])
        .output()
        .unwrap();
    assert!(found.status.success());
    assert!(!strip_ansi(stdout(&found)).contains("No matches found"));

    // --user does NOT find it.
    let miss = world
        .cmd()
        .args(["search", "ASST_UNIQUE_TEXT_AF", "--user", "--project", proj.path()])
        .output()
        .unwrap();
    assert!(strip_ansi(stdout(&miss)).contains("No matches found"));
}

#[test]
fn test_search_bash_command_flag() {
    let world = MockWorld::new();
    let proj = world.project("bash-cmd-flag");
    proj.session("sess-bc")
        .bash("BASH_CMD_UNIQUE_XYZ", "some output")
        .done();

    let out = world
        .cmd()
        .args(["search", "BASH_CMD_UNIQUE_XYZ", "--bash-command", "--project", proj.path()])
        .output()
        .unwrap();

    assert!(out.status.success());
    assert!(!strip_ansi(stdout(&out)).contains("No matches found"),
        "bash-command flag should find the command text");
}

#[test]
fn test_search_bash_output_flag() {
    let world = MockWorld::new();
    let proj = world.project("bash-out-flag");
    proj.session("sess-bo")
        .bash("ls", "BASH_OUTPUT_UNIQUE_QRS")
        .done();

    let out = world
        .cmd()
        .args(["search", "BASH_OUTPUT_UNIQUE_QRS", "--bash-output", "--project", proj.path()])
        .output()
        .unwrap();

    assert!(out.status.success());
    assert!(!strip_ansi(stdout(&out)).contains("No matches found"),
        "bash-output flag should find the command output");
}

#[test]
fn test_search_tool_use_flag() {
    let world = MockWorld::new();
    let proj = world.project("tool-use-flag");
    proj.session("sess-tu")
        .tool("Read", "file_path", "TOOL_USE_UNIQUE_PATH_ABC", "file contents")
        .done();

    let out = world
        .cmd()
        .args(["search", "TOOL_USE_UNIQUE_PATH_ABC", "--tool-use", "--project", proj.path()])
        .output()
        .unwrap();

    assert!(out.status.success());
    assert!(!strip_ansi(stdout(&out)).contains("No matches found"),
        "tool-use flag should find the tool input");
}

#[test]
fn test_search_tool_result_flag() {
    let world = MockWorld::new();
    let proj = world.project("tool-result-flag");
    proj.session("sess-tr")
        .tool("Read", "file_path", "/some/path", "TOOL_RESULT_UNIQUE_DEF")
        .done();

    let out = world
        .cmd()
        .args(["search", "TOOL_RESULT_UNIQUE_DEF", "--tool-result", "--project", proj.path()])
        .output()
        .unwrap();

    assert!(out.status.success());
    assert!(!strip_ansi(stdout(&out)).contains("No matches found"),
        "tool-result flag should find the tool output");
}

#[test]
fn test_search_subagent_prompt_flag() {
    let world = MockWorld::new();
    let proj = world.project("subagent-flag");
    // Parent session (regular, so its messages are "user" type).
    proj.session("parent-sess-sp").user_message("PARENT_MSG").done();
    // Subagent session — its messages become "subagent-prompt".
    proj.subagent_session("parent-sess-sp", "agent-01")
        .user_message("SUBAGENT_UNIQUE_PROMPT_GHI")
        .done();

    // --subagent-prompt finds the subagent's user message.
    let found = world
        .cmd()
        .args(["search", "SUBAGENT_UNIQUE_PROMPT_GHI", "--subagent-prompt",
               "--project", proj.path()])
        .output()
        .unwrap();
    assert!(found.status.success());
    assert!(!strip_ansi(stdout(&found)).contains("No matches found"),
        "subagent-prompt flag should find subagent messages");

    // --user does NOT find subagent messages.
    let miss = world
        .cmd()
        .args(["search", "SUBAGENT_UNIQUE_PROMPT_GHI", "--user", "--project", proj.path()])
        .output()
        .unwrap();
    assert!(strip_ansi(stdout(&miss)).contains("No matches found"),
        "--user should not match subagent prompts");
}

#[test]
fn test_search_compact_summary_flag() {
    let world = MockWorld::new();
    let proj = world.project("compact-flag");
    proj.session("sess-cs")
        .compact_summary("COMPACT_SUM_UNIQUE_JKL")
        .done();

    // --compact-summary finds the summary.
    let found = world
        .cmd()
        .args(["search", "COMPACT_SUM_UNIQUE_JKL", "--compact-summary",
               "--project", proj.path()])
        .output()
        .unwrap();
    assert!(found.status.success());
    assert!(!strip_ansi(stdout(&found)).contains("No matches found"),
        "compact-summary flag should find summaries");

    // --user does NOT find compact summaries.
    let miss = world
        .cmd()
        .args(["search", "COMPACT_SUM_UNIQUE_JKL", "--user", "--project", proj.path()])
        .output()
        .unwrap();
    assert!(strip_ansi(stdout(&miss)).contains("No matches found"),
        "--user should not match compact summaries");
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
        .args(["search", "ONLY_IN_SESSION", "--user",
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
        .args(["search", "TARGET_LINE_BCTX", "--user", "--project", proj.path()])
        .output()
        .unwrap();
    let with_b = world
        .cmd()
        .args(["search", "TARGET_LINE_BCTX", "--user", "-B", "1", "--project", proj.path()])
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
        .args(["search", "TARGET_LINE_ACTX", "--user", "--project", proj.path()])
        .output()
        .unwrap();
    let with_a = world
        .cmd()
        .args(["search", "TARGET_LINE_ACTX", "--user", "-A", "1", "--project", proj.path()])
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
        .args(["search", "LINEWIDTH_SEARCH", "--user", "--project", proj.path()])
        .output()
        .unwrap();
    assert!(default_out.status.success());
    let default_text = strip_ansi(stdout(&default_out));
    assert!(!default_text.contains(tail),
        "default max-line-width should truncate the tail");

    // --max-line-width 0: full line visible.
    let unlimited_out = world
        .cmd()
        .args(["search", "LINEWIDTH_SEARCH", "--user",
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

// =============================================================================
// error handling — full help on incorrect invocations (issue #6)
// =============================================================================

fn stderr(out: &std::process::Output) -> &str {
    std::str::from_utf8(&out.stderr).unwrap()
}

#[test]
fn test_unknown_subcommand_shows_full_help_and_exits_nonzero() {
    let world = MockWorld::new();

    let out = world.cmd().args(["foobar"]).output().unwrap();

    assert!(!out.status.success(), "unknown subcommand should exit nonzero");
    let err = strip_ansi(stderr(&out));
    // Full help must list the available subcommands
    assert!(err.contains("search"), "stderr should contain 'search' subcommand");
    assert!(err.contains("last"), "stderr should contain 'last' subcommand");
    assert!(err.contains("dump"), "stderr should contain 'dump' subcommand");
    assert!(err.contains("projects"), "stderr should contain 'projects' subcommand");
    // The specific error should also appear
    assert!(err.contains("foobar"), "stderr should mention the unrecognized subcommand");
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

// ── --diff flag ───────────────────────────────────────────────────────────────

#[test]
fn test_diff_shows_unified_diff_for_edit_tool() {
    let world = MockWorld::new();
    let proj = world.project("diff-basic");
    proj.session("aaaa")
        .edit("/src/main.rs", "fn old_name() {}\n", "fn new_name() {}\n")
        .done();

    let out = world
        .cmd()
        .args(["search", "old_name", "--tool-use", "--diff", "--project", proj.path()])
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
        .args(["search", "line", "--tool-use", "--diff", "--project", proj.path()])
        .output()
        .unwrap();

    let text = strip_ansi(stdout(&out));
    assert!(text.contains("@@"), "diff output should contain @@ hunk header");
}

#[test]
fn test_diff_without_flag_shows_raw_format() {
    let world = MockWorld::new();
    let proj = world.project("diff-raw");
    proj.session("cccc")
        .edit("/bar.rs", "old content\n", "new content\n")
        .done();

    let out = world
        .cmd()
        .args(["search", "old_string", "--tool-use", "--project", proj.path()])
        .output()
        .unwrap();

    // Without --diff, output should NOT contain unified diff markers
    let text = strip_ansi(stdout(&out));
    assert!(!text.contains("--- a/"), "without --diff should not show unified diff header");
    assert!(!text.contains("+++ b/"), "without --diff should not show unified diff header");
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
        .args(["search", "file_path", "--tool-use", "--diff", "--project", proj.path()])
        .output()
        .unwrap();

    // --diff on a non-Edit tool-use should still render normally (not a diff)
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
        .args(["search", "x = ", "--tool-use", "--diff", "--project", proj.path()])
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
        .args(["search", "alpha", "--tool-use", "--diff", "--project", proj.path()])
        .output()
        .unwrap();

    let text = strip_ansi(stdout(&out));
    // old lines prefixed with -
    assert!(text.contains("-fn alpha() {"), "should show removed fn header");
    assert!(text.contains("-    let x = 1;"), "should show removed body line");
    // new lines prefixed with +
    assert!(text.contains("+fn alpha() {"), "should show added fn header");
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
        .args(["search", "old", "--tool-use", "--diff", "--project", proj.path()])
        .output()
        .unwrap();

    let text = strip_ansi(stdout(&out));
    assert!(text.contains("tool: Edit"), "should show tool name in header");
}

#[test]
fn test_diff_json_output_unaffected_by_diff_flag() {
    let world = MockWorld::new();
    let proj = world.project("diff-json");
    proj.session("hhhh")
        .edit("/y.rs", "before\n", "after\n")
        .done();

    // --diff should not affect --json output (JSON is a separate code path)
    let out = world
        .cmd()
        .args(["search", "before", "--tool-use", "--json", "--project", proj.path()])
        .output()
        .unwrap();

    assert!(out.status.success());
    let text = stdout(&out);
    let parsed: serde_json::Value = serde_json::from_str(text)
        .expect("--json should produce valid JSON regardless of content");
    let arr = parsed.as_array().expect("expected JSON array");
    assert!(!arr.is_empty(), "should have at least one match");
}
