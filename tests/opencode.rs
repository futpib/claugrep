//! Hermetic end-to-end tests for the opencode backend.
//!
//! Each test builds a synthetic opencode SQLite DB (the `session` / `message` /
//! `part` tables) in a temporary directory and invokes the real `claugrep`
//! binary against it via `--opencode-db`. No real opencode install or session
//! data is required — every test is fully deterministic and reproducible,
//! mirroring the `tests/mock.rs` suite's philosophy for the Claude backend.

use std::fs;
use std::path::PathBuf;
use std::process::Command;

extern crate rusqlite;
use rusqlite::{params, Connection};

/// Owns a temp HOME + a synthetic opencode.db. `cmd()` returns a `claugrep`
/// invocation pre-pointed at the fixture DB.
struct OcWorld {
    home: tempfile::TempDir,
    db: PathBuf,
    conn: Connection,
}

/// A non-existent project path (canonicalize falls back to the raw string,
/// exactly like claugrep's `resolve_project` — matches `tests/mock.rs`).
const PROJ: &str = "/claugrep-oc-mock/proj";

impl OcWorld {
    fn new() -> Self {
        let home = tempfile::TempDir::new().unwrap();
        // Place the DB under $HOME/.local/share/opencode so the auto-detect
        // path (`OpenCodeSource::default_db_path`, which honours XDG_DATA_HOME)
        // finds it when we point XDG_DATA_HOME at our mock home.
        let db = home.path().join(".local").join("share").join("opencode").join("opencode.db");
        fs::create_dir_all(db.parent().unwrap()).unwrap();
        let conn = Connection::open(&db).unwrap();
        conn.execute_batch(
            "CREATE TABLE session (\
                id TEXT PRIMARY KEY, project_id TEXT NOT NULL, parent_id TEXT,\
                directory TEXT NOT NULL, time_created INTEGER NOT NULL,\
                time_updated INTEGER NOT NULL);\
             CREATE TABLE message (\
                id TEXT PRIMARY KEY, session_id TEXT NOT NULL,\
                time_created INTEGER NOT NULL, time_updated INTEGER NOT NULL,\
                data TEXT NOT NULL);\
             CREATE TABLE part (\
                id TEXT PRIMARY KEY, message_id TEXT NOT NULL,\
                session_id TEXT NOT NULL, time_created INTEGER NOT NULL,\
                time_updated INTEGER NOT NULL, data TEXT NOT NULL);\
             CREATE INDEX part_session_idx ON part(session_id);",
        )
        .unwrap();
        OcWorld { home, db, conn }
    }

    /// claugrep pinned to the opencode backend + fixture DB + mock project.
    fn cmd(&self) -> Command {
        let mut c = Command::new(env!("CARGO_BIN_EXE_claugrep"));
        c.env("HOME", self.home.path());
        c.env("XDG_DATA_HOME", self.home.path().join(".local").join("share"));
        c.env("XDG_CONFIG_HOME", self.home.path().join(".config"));
        c.args(["--backend", "opencode", "--opencode-db"])
            .arg(&self.db)
            .args(["--project", PROJ]);
        c
    }

    /// claugrep in `--backend auto` mode (no explicit db) — exercises the
    /// auto-detect path against the fixture DB.
    fn cmd_auto(&self) -> Command {
        let mut c = Command::new(env!("CARGO_BIN_EXE_claugrep"));
        c.env("HOME", self.home.path());
        c.env("XDG_DATA_HOME", self.home.path().join(".local").join("share"));
        c.env("XDG_CONFIG_HOME", self.home.path().join(".config"));
        c.args(["--project", PROJ]);
        c
    }

    fn add_session(&self, id: &str, is_sub: bool, ts: i64) {
        self.conn.execute(
            "INSERT INTO session(id, project_id, parent_id, directory, time_created, time_updated) \
             VALUES (?1,'proj',?2,?3,?4,?4)",
            params![id, if is_sub { Some("ses_parent") } else { None }, PROJ, ts],
        ).unwrap();
    }

    fn add_message(&self, id: &str, sid: &str, role: &str, ts: i64) {
        let data = format!("{{\"role\":\"{}\"}}", role);
        self.conn.execute(
            "INSERT INTO message(id, session_id, time_created, time_updated, data) \
             VALUES (?1,?2,?3,?3,?4)",
            params![id, sid, ts, data],
        ).unwrap();
    }

    fn add_part(&self, id: &str, sid: &str, msg: &str, data: serde_json::Value, created: i64, updated: i64) {
        self.conn.execute(
            "INSERT INTO part(id, message_id, session_id, time_created, time_updated, data) \
             VALUES (?1,?2,?3,?4,?5,?6)",
            params![id, msg, sid, created, updated, data.to_string()],
        ).unwrap();
    }

    /// A real on-disk dir under the mock HOME (canonicalized) for `memory`
    /// tests, which walk the actual filesystem for AGENTS.md.
    fn real_project(&self, name: &str) -> PathBuf {
        let p = self.home.path().join("real-projects").join(name);
        fs::create_dir_all(&p).unwrap();
        p.canonicalize().unwrap()
    }
}

/// A canonical session with one user turn, one assistant turn, and one bash call.
fn seed_basic(w: &OcWorld) {
    w.add_session("ses_main", false, 1000);
    w.add_message("msg_u", "ses_main", "user", 1100);
    w.add_message("msg_a", "ses_main", "assistant", 1200);
    w.add_part("p1", "ses_main", "msg_u", serde_json::json!({"type":"text","text":"please search the warp core"}), 1100, 1100);
    w.add_part("p2", "ses_main", "msg_a", serde_json::json!({"type":"reasoning","text":"warp core query planning"}), 1250, 1250);
    w.add_part("p3", "ses_main", "msg_a", serde_json::json!({"type":"text","text":"searching now"}), 1300, 1300);
    w.add_part("p4", "ses_main", "msg_a",
        serde_json::json!({"type":"tool","tool":"bash","state":{"status":"completed","input":{"command":"grep -r warp /ship"},"output":"warp_drive.rs\nwarp_core.rs"}}),
        1400, 1410);
}

// ── sessions ──────────────────────────────────────────────────────────────────

#[test]
fn sessions_lists_project_newest_first() {
    let w = OcWorld::new();
    w.add_session("ses_old", false, 1000);
    w.add_session("ses_new", false, 3000);
    w.add_session("ses_mid", false, 2000);
    let out = w.cmd().args(["sessions"]).output().unwrap();
    assert!(out.status.success(), "stderr: {}", String::from_utf8_lossy(&out.stderr));
    let stdout = String::from_utf8_lossy(&out.stdout);
    let ids: Vec<&str> = stdout.lines()
        .filter_map(|l| l.split_whitespace().last())
        .collect();
    assert_eq!(ids, vec!["ses_new", "ses_mid", "ses_old"]);
}

#[test]
fn sessions_json_has_backend_tag() {
    let w = OcWorld::new();
    w.add_session("ses_x", false, 1000);
    let out = w.cmd().args(["sessions", "--json"]).output().unwrap();
    let arr: serde_json::Value = serde_json::from_str(&String::from_utf8_lossy(&out.stdout)).unwrap();
    assert_eq!(arr[0]["backend"].as_str(), Some("opencode"));
    assert_eq!(arr[0]["sessionId"].as_str(), Some("ses_x"));
}

#[test]
fn sessions_subagents_flag() {
    let w = OcWorld::new();
    w.add_session("ses_parent", false, 1000);
    w.add_session("ses_sub", true, 1100);
    // Hidden without --subagents.
    let out = w.cmd().args(["sessions"]).output().unwrap();
    let s = String::from_utf8_lossy(&out.stdout);
    assert!(!s.contains("ses_sub"), "subagent leaked without --subagents");
    // Shown with --subagents, tagged.
    let out = w.cmd().args(["sessions", "--subagents"]).output().unwrap();
    let s = String::from_utf8_lossy(&out.stdout);
    assert!(s.contains("ses_sub") && s.contains("[subagent]"));
}

// ── search ────────────────────────────────────────────────────────────────────

#[test]
fn search_user_message() {
    let w = OcWorld::new();
    seed_basic(&w);
    let out = w.cmd().args(["search", "warp", "-t", "user"]).output().unwrap();
    assert!(out.status.success());
    let s = String::from_utf8_lossy(&out.stdout);
    assert!(s.contains("warp core") && s.contains("Match #1"));
}

#[test]
fn search_each_target_type() {
    let w = OcWorld::new();
    seed_basic(&w);
    for (term, target, must_contain) in [
        ("warp core", "user", "warp core"),
        ("searching", "assistant", "searching now"),
        ("query planning", "thinking", "warp core query planning"),
        ("grep -r warp", "bash-command", "grep -r warp"),
        ("warp_drive.rs", "bash-output", "warp_drive.rs"),
    ] {
        let out = w.cmd().args(["search", term, "-t", target]).output().unwrap();
        assert!(out.status.success(), "[{}] stderr: {}", target, String::from_utf8_lossy(&out.stderr));
        let s = String::from_utf8_lossy(&out.stdout);
        assert!(s.contains(must_contain), "[{}] expected '{}' in:\n{}", target, must_contain, s);
    }
}

#[test]
fn search_default_targets_reaches_bash_output() {
    // Regression: default search must surface bash OUTPUTS (the 2-slot bug used
    // to drop them when tool-use was also requested, which default search does).
    let w = OcWorld::new();
    seed_basic(&w);
    let out = w.cmd().args(["search", "warp_drive.rs"]).output().unwrap();
    let s = String::from_utf8_lossy(&out.stdout);
    assert!(s.contains("Match #1"), "default search should find the bash output");
}

#[test]
fn search_case_insensitive_subtype() {
    // opencode stamps tool names lowercase ("bash"); the filter must match.
    let w = OcWorld::new();
    seed_basic(&w);
    let out = w.cmd().args(["search", "grep", "-t", "tool-use.bash"]).output().unwrap();
    assert!(out.status.success());
    assert!(String::from_utf8_lossy(&out.stdout).contains("Match #1"));
}

#[test]
fn search_json_envelope_is_portable() {
    let w = OcWorld::new();
    seed_basic(&w);
    let out = w.cmd().args(["search", "warp", "-t", "user", "--json"]).output().unwrap();
    let first: serde_json::Value = serde_json::from_str(
        String::from_utf8_lossy(&out.stdout).lines().next().unwrap(),
    ).unwrap();
    assert_eq!(first["sessionId"], "ses_main");
    assert_eq!(first["type"], "user", "type normalized to role");
    assert_eq!(first["partType"], "text");
    assert_eq!(first["slot"], "text");
    assert!(first["timestamp"].is_string());
    // Bash tool part envelope via its own slot.
    let out = w.cmd().args(["search", "grep", "-t", "bash-command", "--json"]).output().unwrap();
    let v: serde_json::Value = serde_json::from_str(
        String::from_utf8_lossy(&out.stdout).lines().next().unwrap(),
    ).unwrap();
    assert_eq!(v["slot"], "cmd");
    assert_eq!(v["partType"], "tool");
    assert_eq!(v["type"], "assistant");
}

// ── dump ──────────────────────────────────────────────────────────────────────

#[test]
fn dump_targets_filter() {
    let w = OcWorld::new();
    seed_basic(&w);
    let out = w.cmd().args(["dump", "1", "--targets", "assistant"]).output().unwrap();
    assert!(out.status.success());
    let s = String::from_utf8_lossy(&out.stdout);
    // Every record label must be [assistant] (no user/thinking/bash leaking).
    let re = regex::Regex::new(r"^\[[a-z-]+\.").unwrap();
    for line in s.lines() {
        if re.is_match(line) {
            assert!(line.starts_with("[assistant"), "non-assistant leaked: {}", line);
        }
    }
}

#[test]
fn dump_edit_renders_unified_diff() {
    let w = OcWorld::new();
    w.add_session("ses_e", false, 1000);
    w.add_message("msg_a", "ses_e", "assistant", 1200);
    w.add_part("p1", "ses_e", "msg_a",
        serde_json::json!({"type":"tool","tool":"edit","state":{"status":"completed",
            "input":{"filePath":"/warp.rs","oldString":"fn old(){}","newString":"fn new(){}"},
            "output":"done"}}),
        1200, 1210);
    let out = w.cmd().args(["dump", "1", "-t", "tool-use.Edit"]).output().unwrap();
    let s = String::from_utf8_lossy(&out.stdout);
    assert!(s.contains("--- a/warp.rs") && s.contains("+++ b/warp.rs"), "expected diff:\n{}", s);
    assert!(s.contains("-fn old(){}") && s.contains("+fn new(){}"));
}

// ── projects ──────────────────────────────────────────────────────────────────

#[test]
fn projects_lists_with_backend_tag() {
    let w = OcWorld::new();
    w.add_session("ses_a", false, 1000);
    w.add_session("ses_b", false, 2000);
    // auto-detect: only opencode has data here, but the row is still tagged
    // because more than one backend is *enabled* (auto). Use --backend opencode
    // explicitly to get a clean single-backend listing without the tag noise.
    let out = w.cmd().args(["projects"]).output().unwrap();
    let s = String::from_utf8_lossy(&out.stdout);
    assert!(s.contains(PROJ), "project path missing:\n{}", s);
    assert!(s.contains("2 sessions"), "session count missing");
}

// ── date filters ──────────────────────────────────────────────────────────────

#[test]
fn date_filters_bound_the_fixture() {
    let w = OcWorld::new();
    seed_basic(&w); // all rows at epoch-ms 1000..=1410 (year 1970)
    // After 2099 → nothing.
    let out = w.cmd().args(["sessions", "--after", "2099-01-01"]).output().unwrap();
    assert!(!out.status.success(), "future --after should yield no sessions");
    // Before 2099 → the fixture session.
    let out = w.cmd().args(["sessions", "--before", "2099-01-01"]).output().unwrap();
    assert!(out.status.success());
    assert!(String::from_utf8_lossy(&out.stdout).contains("ses_main"));
}

// ── auto-detect ───────────────────────────────────────────────────────────────

#[test]
fn auto_detect_finds_fixture_db() {
    // No --backend / --opencode-db: auto-detect via XDG_DATA_HOME must find
    // the fixture DB under <home>/.local/share/opencode/opencode.db.
    let w = OcWorld::new();
    seed_basic(&w);
    let out = w.cmd_auto().args(["sessions"]).output().unwrap();
    assert!(out.status.success(), "stderr: {}", String::from_utf8_lossy(&out.stderr));
    assert!(String::from_utf8_lossy(&out.stdout).contains("ses_main"));
}

// ── memory (AGENTS.md) ────────────────────────────────────────────────────────

#[test]
fn memory_walks_agents_md() {
    let w = OcWorld::new();
    // Global AGENTS.md under the opencode config dir.
    let cfg = w.home.path().join(".config").join("opencode");
    fs::create_dir_all(&cfg).unwrap();
    fs::write(cfg.join("AGENTS.md"), "# Global rules\nnever commit secrets\n").unwrap();
    // Project AGENTS.md in a real on-disk project.
    let proj = w.real_project("ship");
    fs::write(proj.join("AGENTS.md"), "# Project rules\nprefer trait abstractions\n").unwrap();

    let mut c = w.cmd();
    c.args(["memory", "search", "rules"]).arg("--project").arg(&proj);
    let out = c.output().unwrap();
    let s = String::from_utf8_lossy(&out.stdout);
    assert!(s.contains("Global rules") && s.contains("Project rules"),
        "expected both AGENTS.md files:\n{}", s);
}
