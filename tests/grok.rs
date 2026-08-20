use std::fs;
use std::io::Write;
use std::path::PathBuf;
use std::process::{Command, Output, Stdio};
use std::time::Duration;

use serde_json::{json, Value};
use tempfile::TempDir;

struct GrokWorld {
    home: TempDir,
    project: TempDir,
}

impl GrokWorld {
    fn new() -> Self {
        Self {
            home: tempfile::tempdir().unwrap(),
            project: tempfile::tempdir().unwrap(),
        }
    }

    fn cmd(&self) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_claugrep"));
        command
            .env("HOME", self.home.path())
            .args(["--backend", "grok", "--grok-home"])
            .arg(self.home.path())
            .arg("--project")
            .arg(self.project.path())
            .arg("--color")
            .arg("never");
        command
    }

    fn add_session(
        &self,
        id: &str,
        title: &str,
        manual_title: bool,
        session_kind: Option<&str>,
        updated_at: &str,
        updates: &[Value],
    ) -> PathBuf {
        let dir = self.home.path().join("sessions/project").join(id);
        fs::create_dir_all(&dir).unwrap();
        let summary = json!({
            "info": {
                "id": id,
                "cwd": self.project.path().canonicalize().unwrap(),
            },
            "generated_title": title,
            "session_summary": title,
            "title_is_manual": manual_title,
            "session_kind": session_kind,
            "updated_at": updated_at,
        });
        fs::write(
            dir.join("summary.json"),
            serde_json::to_vec(&summary).unwrap(),
        )
        .unwrap();
        let body = updates
            .iter()
            .map(|update| serde_json::to_string(update).unwrap())
            .collect::<Vec<_>>()
            .join("\n");
        fs::write(dir.join("updates.jsonl"), format!("{}\n", body)).unwrap();
        dir
    }
}

fn update(timestamp: i64, session_id: &str, update: Value) -> Value {
    json!({
        "method": "session/update",
        "params": {
            "sessionId": session_id,
            "update": update,
        },
        "timestamp": timestamp,
    })
}

fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).to_string()
}

fn grok_slug(input: &str) -> String {
    let mut slug = String::new();
    let mut previous_dash = false;
    for character in input.to_lowercase().chars() {
        let character = if character.is_ascii_alphanumeric() {
            character
        } else {
            '-'
        };
        if character == '-' {
            if !previous_dash {
                slug.push(character);
            }
            previous_dash = true;
        } else {
            slug.push(character);
            previous_dash = false;
        }
    }
    slug.chars()
        .take(40)
        .collect::<String>()
        .trim_matches('-')
        .to_string()
}

fn seed_full(world: &GrokWorld) {
    let id = "grok-main";
    world.add_session(
        id,
        "Manually Named Grok Session",
        true,
        None,
        "2026-08-20T12:00:09Z",
        &[
            update(
                1_777_000_001,
                id,
                json!({
                    "sessionUpdate": "user_message_chunk",
                    "content": {"type": "text", "text": "please inspect the nebula drive"},
                }),
            ),
            update(
                1_777_000_002,
                id,
                json!({
                    "sessionUpdate": "agent_thought_chunk",
                    "content": {"type": "text", "text": "reasoning about the nebula drive"},
                }),
            ),
            update(
                1_777_000_003,
                id,
                json!({
                    "sessionUpdate": "agent_message_chunk",
                    "content": {"type": "text", "text": "the nebula drive is ready"},
                }),
            ),
            update(
                1_777_000_004,
                id,
                json!({
                    "sessionUpdate": "tool_call",
                    "toolCallId": "call-shell",
                    "title": "run_terminal_command",
                    "rawInput": {"command": "printf nebula-output", "description": "smoke"},
                    "_meta": {"x.ai/tool": {"name": "run_terminal_command", "kind": "execute"}},
                }),
            ),
            update(
                1_777_000_005,
                id,
                json!({
                    "sessionUpdate": "tool_call_update",
                    "toolCallId": "call-shell",
                    "kind": "execute",
                    "rawInput": {"command": "printf nebula-output"},
                    "_meta": {"x.ai/tool": {"name": "run_terminal_command", "kind": "execute"}},
                }),
            ),
            update(
                1_777_000_006,
                id,
                json!({
                    "sessionUpdate": "tool_call_update",
                    "toolCallId": "call-shell",
                    "status": "completed",
                    "content": [{"type": "content", "content": {"type": "text", "text": "nebula-output"}}],
                    "rawOutput": {"type": "Bash", "output_for_prompt": "exit: 0\nnebula-output"},
                }),
            ),
            update(
                1_777_000_007,
                id,
                json!({
                    "sessionUpdate": "tool_call",
                    "toolCallId": "call-edit",
                    "title": "search_replace",
                    "rawInput": {
                        "file_path": "/repo/drive.rs",
                        "old_string": "fn old() {}",
                        "new_string": "fn nebula() {}"
                    },
                    "_meta": {"x.ai/tool": {"name": "search_replace", "kind": "edit"}},
                }),
            ),
            update(
                1_777_000_008,
                id,
                json!({
                    "sessionUpdate": "session_recap",
                    "summary": "The nebula drive inspection is complete.",
                }),
            ),
        ],
    );
}

#[test]
fn searches_messages_tools_diffs_recaps_titles_and_json() {
    let world = GrokWorld::new();
    seed_full(&world);

    for (pattern, target, expected) in [
        ("please inspect", "user", "nebula drive"),
        ("reasoning about", "thinking", "reasoning about the nebula"),
        ("drive is ready", "assistant", "the nebula drive is ready"),
        ("printf nebula", "bash-command", "printf nebula-output"),
        ("nebula-output", "bash-output", "nebula-output"),
        (
            "inspection is complete",
            "compact-summary",
            "inspection is complete",
        ),
        (
            "Manually Named",
            "custom-title",
            "Manually Named Grok Session",
        ),
    ] {
        let output = world
            .cmd()
            .args(["search", pattern, "-t", target])
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "[{target}] stderr: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(
            stdout(&output).contains(expected),
            "[{target}] {}",
            stdout(&output)
        );
    }

    let edit = world
        .cmd()
        .args(["search", "nebula", "-t", "tool-use.search_replace"])
        .output()
        .unwrap();
    let edit = stdout(&edit);
    assert!(edit.contains("--- a/repo/drive.rs"));
    assert!(edit.contains("-fn old() {}") && edit.contains("+fn nebula() {}"));

    let json_output = world
        .cmd()
        .args(["search", "please inspect", "-t", "user", "--json"])
        .output()
        .unwrap();
    let record: Value = serde_json::from_str(stdout(&json_output).lines().next().unwrap()).unwrap();
    assert_eq!(record["sessionId"], "grok-main");
    assert_eq!(record["type"], "user");
    assert_eq!(record["itemType"], "user_message_chunk");
    assert!(record["timestamp"].is_string());
}

#[test]
fn discovers_projects_titles_and_subagents() {
    let world = GrokWorld::new();
    seed_full(&world);
    world.add_session(
        "grok-child",
        "Child task",
        false,
        Some("subagent"),
        "2026-08-20T12:00:10Z",
        &[update(
            1_777_000_010,
            "grok-child",
            json!({
                "sessionUpdate": "user_message_chunk",
                "content": {"type": "text", "text": "subagent nebula assignment"},
            }),
        )],
    );

    let sessions = world.cmd().arg("sessions").output().unwrap();
    let sessions = stdout(&sessions);
    assert!(sessions.contains("grok-main") && sessions.contains("Manually Named Grok Session"));
    assert!(!sessions.contains("grok-child"));

    let sessions = world
        .cmd()
        .args(["sessions", "--subagents", "--json"])
        .output()
        .unwrap();
    let sessions: Value = serde_json::from_slice(&sessions.stdout).unwrap();
    assert_eq!(sessions.as_array().unwrap().len(), 2);
    assert_eq!(sessions[0]["backend"], "grok");
    assert!(sessions.as_array().unwrap().iter().any(|row| {
        row["sessionId"] == "grok-child" && row["isSubagent"] == true && row["title"].is_null()
    }));

    let child = world
        .cmd()
        .args([
            "search",
            "subagent nebula",
            "-t",
            "subagent-prompt",
            "--subagents",
        ])
        .output()
        .unwrap();
    assert!(child.status.success());
    assert!(stdout(&child).contains("subagent nebula assignment"));

    let projects = world.cmd().args(["projects", "--json"]).output().unwrap();
    let projects: Value = serde_json::from_slice(&projects.stdout).unwrap();
    assert_eq!(projects[0]["backend"], "grok");
    assert_eq!(projects[0]["sessionCount"], 2);
}

#[test]
fn searches_grok_rules_and_native_memory() {
    let world = GrokWorld::new();
    assert!(Command::new("git")
        .args(["init", "-q"])
        .current_dir(world.project.path())
        .status()
        .unwrap()
        .success());
    assert!(Command::new("git")
        .args(["remote", "add", "origin", "git@github.com:acme/nebula.git",])
        .current_dir(world.project.path())
        .status()
        .unwrap()
        .success());
    fs::write(
        world.home.path().join("AGENTS.md"),
        "global grok rule marker\n",
    )
    .unwrap();
    fs::write(
        world.project.path().join("AGENTS.md"),
        "project grok rule marker\n",
    )
    .unwrap();
    fs::create_dir_all(world.home.path().join("rules")).unwrap();
    fs::write(
        world.home.path().join("rules/global.md"),
        "global grok rules directory marker\n",
    )
    .unwrap();
    fs::create_dir_all(world.project.path().join(".grok/rules")).unwrap();
    fs::write(
        world.project.path().join(".grok/rules/project.md"),
        "project grok rules directory marker\n",
    )
    .unwrap();

    let memory_root = world.home.path().join("memory");
    fs::create_dir_all(&memory_root).unwrap();
    fs::write(memory_root.join("MEMORY.md"), "global grok memory marker\n").unwrap();
    let identity = "acme/nebula";
    let leaf = grok_slug("nebula");
    let hash = blake3::hash(identity.as_bytes());
    let workspace = memory_root.join(format!("{}-{}", leaf, &hash.to_hex()[..8]));
    fs::create_dir_all(workspace.join("sessions")).unwrap();
    fs::write(
        workspace.join("MEMORY.md"),
        "workspace grok memory marker\n",
    )
    .unwrap();
    fs::write(
        workspace.join("sessions/log.md"),
        "session grok memory marker\n",
    )
    .unwrap();

    for marker in [
        "global grok rule",
        "project grok rule",
        "global grok rules directory",
        "project grok rules directory",
        "global grok memory",
        "workspace grok memory",
        "session grok memory",
    ] {
        let output = world
            .cmd()
            .args(["memory", "search", marker, "--no-subdirs"])
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "[{marker}] {}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(
            stdout(&output).contains(marker),
            "[{marker}] {}",
            stdout(&output)
        );
    }
}

#[test]
fn explicit_backend_reports_missing_home() {
    let home = tempfile::tempdir().unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_claugrep"))
        .args(["--backend", "grok", "--grok-home"])
        .arg(home.path())
        .arg("sessions")
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&output.stderr).contains("--grok-home"));
}

#[test]
fn auto_backend_detects_explicit_grok_home() {
    let world = GrokWorld::new();
    seed_full(&world);
    let output = Command::new(env!("CARGO_BIN_EXE_claugrep"))
        .env("HOME", world.home.path())
        .args(["--grok-home"])
        .arg(world.home.path())
        .arg("--project")
        .arg(world.project.path())
        .arg("search")
        .arg("please inspect")
        .output()
        .unwrap();
    assert!(output.status.success());
    assert!(stdout(&output).contains("nebula drive"));
}

#[test]
fn tail_follow_reads_appended_grok_updates() {
    let world = GrokWorld::new();
    let session_dir = world.add_session(
        "grok-follow",
        "Follow test",
        false,
        None,
        "2026-08-20T12:00:00Z",
        &[update(
            1_777_100_000,
            "grok-follow",
            json!({
                "sessionUpdate": "user_message_chunk",
                "content": {"type": "text", "text": "initial record"},
            }),
        )],
    );
    let mut child = world
        .cmd()
        .args(["tail", "-f", "-n", "0", "-t", "assistant", "grok-follow"])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    std::thread::sleep(Duration::from_millis(400));

    let mut file = fs::OpenOptions::new()
        .append(true)
        .open(session_dir.join("updates.jsonl"))
        .unwrap();
    writeln!(
        file,
        "{}",
        update(
            1_777_100_001,
            "grok-follow",
            json!({
                "sessionUpdate": "agent_message_chunk",
                "content": {"type": "text", "text": "GROK_FOLLOW_APPENDED"},
            }),
        )
    )
    .unwrap();
    drop(file);
    std::thread::sleep(Duration::from_millis(500));

    child.kill().unwrap();
    let output = child.wait_with_output().unwrap();
    assert!(stdout(&output).contains("GROK_FOLLOW_APPENDED"));
}
