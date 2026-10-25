//! CLI regressions using the OpenCode v2.1.2 SQLite/message schema. These
//! fixtures are synthetic; live database validation is separate.

use std::process::{Command, Output};

use rusqlite::{params, Connection};
use serde_json::{json, Value};

const PROJ: &str = "/claugrep-v2-fixture";

struct World {
    home: tempfile::TempDir,
    db: std::path::PathBuf,
    conn: Connection,
}

impl World {
    fn new() -> Self {
        let home = tempfile::tempdir().unwrap();
        let db = home.path().join("opencode/opencode.db");
        std::fs::create_dir_all(db.parent().unwrap()).unwrap();
        let conn = Connection::open(&db).unwrap();
        conn.execute_batch("CREATE TABLE session_v2 (
            id TEXT PRIMARY KEY, directory TEXT NOT NULL, parent_id TEXT,
            title TEXT, time_created INTEGER NOT NULL, time_updated INTEGER NOT NULL);
            CREATE TABLE session_message (
            id TEXT PRIMARY KEY, session_id TEXT NOT NULL, type TEXT NOT NULL,
            seq INTEGER NOT NULL, data TEXT NOT NULL, time_created INTEGER NOT NULL, time_updated INTEGER NOT NULL);
            CREATE UNIQUE INDEX session_message_session_seq_idx ON session_message(session_id,seq);").unwrap();
        Self { home, db, conn }
    }

    fn cmd(&self) -> Command {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_claugrep"));
        cmd.env("HOME", self.home.path())
            .env("XDG_DATA_HOME", self.home.path())
            .env("XDG_CONFIG_HOME", self.home.path().join("config"))
            .args(["--backend", "opencode", "--project", PROJ]);
        cmd
    }

    fn run(&self, args: &[&str]) -> Output {
        let out = self.cmd().args(args).output().unwrap();
        assert!(
            out.status.success(),
            "{args:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        out
    }

    fn session(&self, id: &str, parent: Option<&str>, updated: i64) {
        self.conn
            .execute(
                "INSERT INTO session_v2 VALUES (?1,?2,?3,'Warp investigation',1000,?4)",
                params![id, PROJ, parent, updated],
            )
            .unwrap();
    }

    fn message(&self, id: &str, sid: &str, kind: &str, seq: i64, data: Value) {
        self.conn
            .execute(
                "INSERT INTO session_message VALUES (?1,?2,?3,?4,?5,?6,?6)",
                params![id, sid, kind, seq, data.to_string(), 1000 + seq],
            )
            .unwrap();
    }

    fn seed(&self) {
        self.session("ses_v2", None, 3000);
        // Insertion/rowid order intentionally differs from sequence order.
        self.message("msg_a", "ses_v2", "assistant", 20, json!({
            "agent":"build", "time":{"created":1020,"streamed":1040,"completed":1060},
            "content":[
                {"type":"reasoning","text":"trace warp failure","time":{"created":1021,"completed":1022}},
                {"type":"text","text":"first answer"},
                {"type":"tool","id":"call_shell","name":"shell","time":{"created":1030,"ran":1031,"completed":1050},
                 "state":{"status":"completed","input":{"command":"cat warp.log"},
                    "content":[{"type":"text","text":"warp log line one"},{"type":"text","text":"warp log line two"}]}},
                {"type":"tool","id":"call_edit","name":"edit","time":{"created":1032,"completed":1051},
                 "state":{"status":"completed","input":{"filePath":"/warp.rs","oldString":"old_code","newString":"new_code"},
                    "content":[{"type":"text","text":"edit complete"}]}},
                {"type":"text","text":"second answer"}
            ]
        }));
        self.message(
            "msg_u",
            "ses_v2",
            "user",
            10,
            json!({"text":"inspect the warp core", "time":{"created":1010}}),
        );
    }
}

fn records(out: &Output) -> Vec<Value> {
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect()
}

#[test]
fn v2_only_database_discovery_dump_tail_and_last() {
    let w = World::new();
    w.seed();
    let out = w.run(&["sessions", "--json"]);
    let sessions: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(sessions[0]["sessionId"], "ses_v2");
    assert_eq!(sessions[0]["title"], "Warp investigation");
    assert!(String::from_utf8_lossy(&w.run(&["projects"]).stdout).contains("1 session"));
    let dump = records(&w.run(&["dump", "ses_v2", "--json", "-t", "user,assistant"]));
    assert_eq!(dump.len(), 3);
    assert_eq!(dump[0]["id"], "msg_u");
    assert_eq!(dump[1]["contentIndex"], 1);
    assert_eq!(dump[2]["contentIndex"], 4);
    assert_eq!(dump[1]["itemType"], "assistant");
    let tail = records(&w.run(&["tail", "ses_v2", "-n", "1", "--json", "-t", "assistant"]));
    assert_eq!(tail.len(), 1);
    assert_eq!(tail[0]["contentIndex"], 4);
    let last = records(&w.run(&["last", "-n", "1", "-t", "bash-output", "--json"]));
    assert_eq!(last.len(), 1);
    assert_eq!(last[0]["slot"], "out");
    assert_eq!(
        last[0]["content"][2]["state"]["content"][1]["text"],
        "warp log line two"
    );
    let out = w
        .cmd()
        .args(["sessions", "--after", "2099-01-01"])
        .output()
        .unwrap();
    assert!(!out.status.success());
}

#[test]
fn v2_targets_tools_errors_and_summaries() {
    let w = World::new();
    w.seed();
    w.message(
        "msg_c",
        "ses_v2",
        "compaction",
        30,
        json!({"status":"completed","summary":"warp recap", "metadata":{"recent":"kept context"}}),
    );
    w.message(
        "msg_s",
        "ses_v2",
        "synthetic",
        40,
        json!({"text":"monitor wake"}),
    );
    w.message("msg_bg", "ses_v2", "shell", 50, json!({"command":"echo background", "status":"exited", "output":{"output":"background result"}}));
    w.message("msg_error", "ses_v2", "assistant", 60, json!({"time":{"completed":1070},"content":[
        {"type":"tool","name":"read","id":"error1","state":{"status":"error","input":{"filePath":"/missing"},
            "error":{"message":"file not found"}}}
    ]}));
    for (term, target) in [
        ("warp core", "user"),
        ("monitor wake", "user"),
        ("first answer", "assistant"),
        ("second answer", "assistant"),
        ("trace warp", "thinking"),
        ("cat warp", "bash-command.shell"),
        ("warp log line two", "bash-output.shell"),
        ("cat warp", "tool-use.shell"),
        ("edit complete", "tool-result.edit"),
        ("file not found", "tool-result.read"),
        ("warp recap", "compact-summary"),
        ("kept context", "compact-summary"),
        ("echo background", "bash-command"),
        ("background result", "bash-output"),
    ] {
        let out = w.run(&["search", term, "-t", target]);
        assert!(
            String::from_utf8_lossy(&out.stdout).contains("Match #1"),
            "missing {target}: {term}"
        );
    }
    let out = w.run(&["dump", "ses_v2", "-t", "tool-use.Edit"]);
    let s = String::from_utf8_lossy(&out.stdout);
    assert!(s.contains("--- a/warp.rs") && s.contains("-old_code") && s.contains("+new_code"));
    let raw = records(&w.run(&["search", "warp log line two", "--json", "-t", "bash-output"]));
    assert_eq!(raw[0]["timestamp"], "1970-01-01T00:00:01.050Z");
    assert_eq!(raw[0]["slot"], "out");
    assert_eq!(raw[0]["content"][2]["name"], "shell");
}

#[test]
fn migrated_sessions_prefer_v2_and_keep_unmigrated_v1() {
    let w = World::new();
    w.seed();
    w.conn.execute_batch("CREATE TABLE session (id TEXT PRIMARY KEY, directory TEXT, parent_id TEXT, title TEXT, time_updated INTEGER);
        CREATE TABLE message (id TEXT PRIMARY KEY, data TEXT);
        CREATE TABLE part (id TEXT PRIMARY KEY, message_id TEXT, session_id TEXT, data TEXT, time_created INTEGER, time_updated INTEGER);").unwrap();
    for id in ["ses_v2", "ses_old"] {
        w.conn
            .execute(
                "INSERT INTO session VALUES (?1,?2,NULL,'old title',9000)",
                params![id, PROJ],
            )
            .unwrap();
        w.conn
            .execute(
                "INSERT INTO message VALUES (?1,'{\"role\":\"user\"}')",
                [id],
            )
            .unwrap();
        w.conn
            .execute(
                "INSERT INTO part VALUES (?1,?1,?1,?2,1000,1000)",
                params![
                    id,
                    json!({"type":"text","text":format!("legacy {id}")}).to_string()
                ],
            )
            .unwrap();
    }
    let out = w.run(&["sessions", "--json"]);
    let sessions: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(sessions.as_array().unwrap().len(), 2);
    assert_eq!(sessions[0]["sessionId"], "ses_old");
    assert_eq!(sessions[1]["title"], "Warp investigation");
    assert_eq!(sessions[1]["mtime"], 3);
    assert!(String::from_utf8_lossy(&w.run(&["projects"]).stdout).contains("2 sessions"));
    let out = w.run(&["search", "legacy", "--json"]);
    assert_eq!(records(&out).len(), 1);
    assert_eq!(records(&out)[0]["sessionId"], "ses_old");
    // A move in v2 must not expose the stale directory from v1.
    w.conn
        .execute(
            "UPDATE session_v2 SET directory='/moved' WHERE id='ses_v2'",
            [],
        )
        .unwrap();
    let out = w.run(&["sessions", "--json"]);
    let sessions: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(sessions.as_array().unwrap().len(), 1);
}

#[test]
fn v2_subagents_metadata_and_malformed_rows() {
    let w = World::new();
    w.session("ses_parent", None, 1000);
    w.session("ses_child", Some("ses_parent"), 2000);
    w.message(
        "msg_u",
        "ses_child",
        "user",
        1,
        json!({"text":"child task"}),
    );
    w.message(
        "msg_sys",
        "ses_parent",
        "system",
        2,
        json!({"text":"instruction update"}),
    );
    w.message(
        "msg_skill",
        "ses_parent",
        "skill",
        3,
        json!({"text":"skill instructions"}),
    );
    w.message(
        "msg_agent",
        "ses_parent",
        "agent-switched",
        4,
        json!({"agent":"plan"}),
    );
    w.message(
        "msg_model",
        "ses_parent",
        "model-switched",
        5,
        json!({"model":{"id":"example-model"}}),
    );
    let out = w.run(&["sessions", "--json"]);
    assert!(!String::from_utf8_lossy(&out.stdout).contains("ses_child"));
    let out = w.run(&[
        "search",
        "child task",
        "--subagents",
        "-t",
        "subagent-prompt",
        "--json",
    ]);
    assert_eq!(records(&out)[0]["sessionId"], "ses_child");
    for (term, target) in [
        ("instruction update", "system.system"),
        ("skill instructions", "system.skill"),
        ("plan", "agent-name"),
        ("example-model", "system.model-switched"),
    ] {
        let out = w.run(&["search", term, "-t", target]);
        assert!(String::from_utf8_lossy(&out.stdout).contains("Match #1"));
    }
    w.conn
        .execute(
            "INSERT INTO session_message VALUES ('bad','ses_parent','user',6,'{broken',1000,1000)",
            [],
        )
        .unwrap();
    let out = w.run(&["dump", "ses_parent", "-t", "all"]);
    assert!(String::from_utf8_lossy(&out.stderr).contains("message bad: invalid"));
    assert!(String::from_utf8_lossy(&out.stdout).contains("instruction update"));
}

#[test]
fn v2_auto_detection_and_explicit_database_override() {
    let w = World::new();
    w.seed();
    let out = Command::new(env!("CARGO_BIN_EXE_claugrep"))
        .env("HOME", w.home.path())
        .env("XDG_DATA_HOME", w.home.path())
        .env("XDG_CONFIG_HOME", w.home.path().join("config"))
        .env_remove("CODEX_HOME")
        .env_remove("GROK_HOME")
        .env_remove("CLAUDE_CONFIG_DIR")
        .args(["--project", PROJ, "sessions"])
        .output()
        .unwrap();
    assert!(out.status.success());
    assert!(String::from_utf8_lossy(&out.stdout).contains("ses_v2"));
    let out = w
        .cmd()
        .env("XDG_DATA_HOME", w.home.path().join("absent"))
        .arg("--opencode-db")
        .arg(&w.db)
        .args(["dump", "ses_v2"])
        .output()
        .unwrap();
    assert!(out.status.success());
    assert!(String::from_utf8_lossy(&out.stdout).contains("second answer"));
}

#[test]
fn v2_streaming_input_empty_arguments_and_file_references() {
    let w = World::new();
    w.session("ses_v2", None, 1000);
    w.message(
        "msg_u",
        "ses_v2",
        "user",
        1,
        json!({"text":"inspect file",
        "files":[{"uri":"file:///warp.rs","name":"warp.rs","mime":"text/plain"}]}),
    );
    w.message("msg_a", "ses_v2", "assistant", 2, json!({"content":[
        {"type":"tool","name":"unfinished","id":"streaming", "state":{"status":"streaming","input":"{\"incomplete"}},
        {"type":"tool","name":"monitor_list","id":"empty", "state":{"status":"completed","input":{},
            "content":[{"type":"text","text":"no monitors"}]}},
        {"type":"tool","name":"read","id":"media", "state":{"status":"completed","input":{"filePath":"/image.png"},
            "content":[{"type":"file","uri":"data:image/png;base64,AA==","mime":"image/png"}]}}
    ]}));
    let calls = records(&w.run(&["dump", "ses_v2", "--json", "-t", "tool-use"]));
    assert_eq!(calls.len(), 2);
    assert_eq!(calls[0]["contentIndex"], 1);
    assert_eq!(calls[1]["contentIndex"], 2);
    let out = w.run(&["search", "warp.rs", "-t", "attachment.file"]);
    assert!(String::from_utf8_lossy(&out.stdout).contains("Match #1"));
    let output = records(&w.run(&["dump", "ses_v2", "--json", "-t", "tool-result"]));
    assert_eq!(output.len(), 1);
    assert_eq!(output[0]["contentIndex"], 1);
}

#[test]
fn v2_cli_follow_emits_completed_updates_without_replaying_history() {
    use std::io::{BufRead, BufReader};
    use std::process::{Child, Stdio};
    use std::sync::mpsc;
    use std::time::Duration;

    struct Running(Child);
    impl Drop for Running {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }

    let w = World::new();
    w.session("ses_live", None, 1000);
    w.message(
        "msg_old",
        "ses_live",
        "user",
        1,
        json!({"text":"initial record"}),
    );
    let mut pending = json!({"time":{"created":1002}, "content":[
        {"type":"text","text":"partial"},
        {"type":"tool","name":"shell","id":"tool1","state":{"status":"running","input":{"command":"echo done"}}}
    ]});
    w.message("msg_pending", "ses_live", "assistant", 2, pending.clone());
    let mut child = Running(
        w.cmd()
            .args([
                "tail",
                "ses_live",
                "-n",
                "10",
                "-f",
                "--json",
                "-t",
                "user,assistant,bash-output",
            ])
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .unwrap(),
    );
    let stdout = child.0.stdout.take().unwrap();
    let (tx, rx) = mpsc::channel();
    let reader = std::thread::spawn(move || {
        for line in BufReader::new(stdout).lines() {
            if tx.send(line.unwrap()).is_err() {
                break;
            }
        }
    });
    let read = || -> Value {
        serde_json::from_str(&rx.recv_timeout(Duration::from_secs(5)).unwrap()).unwrap()
    };
    assert_eq!(read()["id"], "msg_old");
    assert_eq!(read()["id"], "msg_pending"); // Initial tail includes the current partial snapshot.
                                             // Allow follow to seed its completed facets before modifying the fixture.
    assert!(rx.recv_timeout(Duration::from_millis(500)).is_err());
    pending["content"][0]["text"] = json!("finished text");
    pending["time"]["streamed"] = json!(2000);
    pending["content"][1]["state"]["status"] = json!("completed");
    pending["content"][1]["state"]["content"] = json!([{"type":"text","text":"done"}]);
    w.conn
        .execute(
            "UPDATE session_message SET data=?1,time_updated=2000 WHERE id='msg_pending'",
            [pending.to_string()],
        )
        .unwrap();
    let text = read();
    let output = read();
    assert_eq!(text["content"][0]["text"], "finished text");
    assert_eq!(text["slot"], "text");
    assert_eq!(output["slot"], "out");
    w.message(
        "msg_new",
        "ses_live",
        "user",
        3,
        json!({"text":"new prompt"}),
    );
    assert_eq!(read()["id"], "msg_new");
    assert!(rx.recv_timeout(Duration::from_millis(500)).is_err());
    drop(child);
    reader.join().unwrap();
}
