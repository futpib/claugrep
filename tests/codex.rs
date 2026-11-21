//! Hermetic coverage of Codex store discovery and streaming searches.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use serde_json::{json, Value};

struct World {
    root: tempfile::TempDir,
    active: PathBuf,
    default: PathBuf,
    project: PathBuf,
}

impl World {
    fn new() -> Self {
        let root = tempfile::tempdir().unwrap();
        let active = root.path().join(".codex-account");
        let default = root.path().join(".codex");
        let project = root.path().join("project");
        fs::create_dir_all(&project).unwrap();
        Self {
            root,
            active,
            default,
            project,
        }
    }

    fn rollout(&self, home: &Path, id: &str, records: &[Value]) {
        let sessions = home.join("sessions/2026/11/21");
        fs::create_dir_all(&sessions).unwrap();
        let meta = json!({"type": "session_meta", "payload": {
            "id": id, "cwd": self.project, "thread_source": "user"
        }});
        let text = std::iter::once(&meta)
            .chain(records)
            .map(|v| format!("{v}\n"))
            .collect::<String>();
        fs::write(sessions.join(format!("rollout-{id}.jsonl")), text).unwrap();
    }

    fn cmd(&self) -> Command {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_claugrep"));
        cmd.env("HOME", self.root.path())
            .env("CODEX_HOME", &self.active)
            .env("NO_COLOR", "1")
            .args(["--backend", "codex", "--project"])
            .arg(&self.project);
        cmd
    }
}

fn message(text: &str) -> Value {
    json!({"type": "response_item", "timestamp": "2026-11-21T17:38:19Z", "payload": {
        "type": "message", "role": "assistant", "content": [{"type": "output_text", "text": text}]
    }})
}

fn output_json(cmd: &mut Command) -> Vec<Value> {
    let out = cmd.output().unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8(out.stdout)
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect()
}

#[test]
fn default_search_includes_active_and_conventional_home() {
    let world = World::new();
    let phrase = "The no-JIT Android run has reached Sky Force’s briefing";
    world.rollout(&world.active, "active", &[message(phrase)]);
    world.rollout(&world.default, "default", &[message(phrase)]);
    let matches =
        output_json(
            world
                .cmd()
                .args(["--json", "search", "JIT Android run has reached Sky"]),
        );
    assert_eq!(matches.len(), 2);
    let ids: std::collections::HashSet<_> = matches
        .iter()
        .map(|m| m["sessionId"].as_str().unwrap())
        .collect();
    assert_eq!(ids, ["active", "default"].into_iter().collect());
}

#[test]
fn explicit_home_restricts_discovery() {
    let world = World::new();
    world.rollout(&world.active, "active", &[message("needle")]);
    world.rollout(&world.default, "default", &[message("needle")]);
    for (home, expected) in [(&world.active, "active"), (&world.default, "default")] {
        let matches = output_json(
            world
                .cmd()
                .arg("--codex-home")
                .arg(home)
                .args(["--json", "search", "needle"]),
        );
        assert_eq!(matches.len(), 1);
        assert_eq!(matches[0]["sessionId"], expected);
    }
}

#[test]
fn missing_active_home_does_not_hide_default_sessions() {
    let world = World::new();
    world.rollout(&world.default, "default", &[message("needle")]);
    let matches = output_json(world.cmd().args(["--json", "search", "needle"]));
    assert_eq!(matches.len(), 1);
    assert_eq!(matches[0]["sessionId"], "default");
}

#[cfg(unix)]
#[test]
fn symlinked_account_does_not_duplicate_sessions() {
    let world = World::new();
    world.rollout(&world.default, "default", &[message("needle")]);
    std::os::unix::fs::symlink(&world.default, &world.active).unwrap();
    let matches = output_json(world.cmd().args(["--json", "search", "needle"]));
    assert_eq!(matches.len(), 1);
}

#[test]
fn stream_preserves_tool_correlation_and_record_context() {
    let world = World::new();
    world.rollout(&world.active, "tools", &[
        message("before"),
        json!({"type": "response_item", "payload": {
            "type": "custom_tool_call", "name": "exec", "call_id": "call-1",
            "input": "text(await tools.exec_command({\"cmd\":\"printf hello\"}));"
        }}),
        json!({"type": "response_item", "payload": {
            "type": "custom_tool_call_output", "call_id": "call-1", "output": "needle\nsecond line"
        }}),
        message("after"),
    ]);
    let matches = output_json(world.cmd().args([
        "--json",
        "-t",
        "bash-output.exec_command",
        "search",
        "needle",
    ]));
    assert_eq!(matches.len(), 1);
    assert_eq!(matches[0]["type"], "bash-output");
    assert_eq!(matches[0]["itemType"], "response_item");
    assert_eq!(matches[0]["payload"]["output"], "needle\nsecond line");

    let out = world
        .cmd()
        .args([
            "-t",
            "bash-output.exec_command",
            "search",
            "needle",
            "--around-records",
            "1",
            "--records-type",
            "assistant",
        ])
        .output()
        .unwrap();
    assert!(out.status.success());
    let text = String::from_utf8(out.stdout).unwrap();
    assert!(text.contains("before"), "{text}");
    assert!(text.contains("after"), "{text}");
}

#[test]
fn result_limit_and_line_context_work_while_streaming() {
    let world = World::new();
    world.rollout(
        &world.active,
        "limits",
        &[message("before\nneedle\nafter"), message("needle again")],
    );
    let out = world
        .cmd()
        .args(["--max-results", "1", "search", "needle", "-C", "1"])
        .output()
        .unwrap();
    assert!(out.status.success());
    let text = String::from_utf8(out.stdout).unwrap();
    assert!(text.contains("before") && text.contains("after"), "{text}");
    assert!(!text.contains("needle again"), "{text}");
    assert!(String::from_utf8_lossy(&out.stderr).contains("Result limit reached"));
}

#[test]
fn prefilter_matches_full_decoding_for_escapes_numbers_and_generated_text() {
    let world = World::new();
    let escaped: Value = serde_json::from_str(r#"{"type":"response_item","payload":{"type":"function_call","name":"exec_command","call_id":"escaped","arguments":"{\"cmd\":\"printf \\u006eeedle\"}"}}"#).unwrap();
    world.rollout(
        &world.active,
        "escaping",
        &[
            message("needle"),
            message("NEEDLE"),
            message("(needle)"),
            escaped,
            json!({"type": "event_msg", "payload": {"type": "context_compacted"}}),
        ],
    );
    let path = world
        .active
        .join("sessions/2026/11/21/rollout-escaping.jsonl");
    use std::io::Write;
    let mut file = fs::OpenOptions::new().append(true).open(path).unwrap();
    // Preserve the raw Unicode escape and exponent spelling on disk.
    writeln!(file, r#"{{"type":"response_item","payload":{{"type":"message","role":"assistant","content":[{{"text":"\u006eeedle"}}]}}}}"#).unwrap();
    writeln!(
        file,
        r#"{{"type":"response_item","payload":{{"type":"function_call_output","output":1E3}}}}"#
    )
    .unwrap();

    for args in [
        vec!["needle"],
        vec!["NEEDLE", "-i"],
        vec!["n[e]+dle"],
        vec!["(needle)", "-F"],
        vec!["context compacted"],
        vec!["1000"],
    ] {
        let streamed = output_json(
            world
                .cmd()
                .args(["--json", "-t", "all", "search"])
                .args(&args),
        );
        let buffered = output_json(
            world
                .cmd()
                .args(["--json", "-t", "all", "search"])
                .args(&args)
                .args(["--after-records", "1"]),
        );
        let buffered: Vec<_> = buffered.into_iter().map(|v| v["match"].clone()).collect();
        assert_eq!(streamed, buffered, "{args:?}");
        assert!(!streamed.is_empty(), "{args:?}");
        if args == ["needle"] {
            assert_eq!(streamed.len(), 4); // two messages, wrapped command, literal punctuation
        }
    }
}
