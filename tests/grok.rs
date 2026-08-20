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

    fn add_summary_fields(&self, id: &str, fields: Value) {
        let path = self
            .home
            .path()
            .join("sessions/project")
            .join(id)
            .join("summary.json");
        let mut summary: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        summary
            .as_object_mut()
            .unwrap()
            .extend(fields.as_object().unwrap().clone());
        fs::write(path, serde_json::to_vec(&summary).unwrap()).unwrap();
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
fn maps_grok_lifecycle_updates_and_full_checkpoint_summary() {
    let world = GrokWorld::new();
    let id = "grok-lifecycle";
    let session_dir = world.add_session(
        id,
        "Lifecycle test",
        false,
        None,
        "2026-08-20T12:00:20Z",
        &[
            update(
                1_777_000_020,
                id,
                json!({
                    "sessionUpdate": "plan",
                    "entries": [{"content": "PARITY_PLAN_MARKER", "status": "in_progress", "priority": "high"}],
                }),
            ),
            update(
                1_777_000_021,
                id,
                json!({
                    "sessionUpdate": "hook_execution",
                    "event_name": "PostToolUse",
                    "tool_name": "search_replace",
                    "runs": [{
                        "name": "parity-hook",
                        "status": {"status": "completed", "elapsed_ms": 12},
                        "output": "PARITY_HOOK_OUTPUT"
                    }],
                }),
            ),
            update(
                1_777_000_022,
                id,
                json!({
                    "sessionUpdate": "retry_state",
                    "type": "retrying",
                    "attempt": 2,
                    "max_retries": 4,
                    "reason": "PARITY_RETRY_REASON",
                }),
            ),
            update(
                1_777_000_023,
                id,
                json!({
                    "sessionUpdate": "task_backgrounded",
                    "task_id": "task-parity",
                    "description": "PARITY_BACKGROUND_START",
                    "command": "cargo test",
                    "cwd": "/repo",
                }),
            ),
            update(
                1_777_000_024,
                id,
                json!({
                    "sessionUpdate": "task_completed",
                    "task_snapshot": {
                        "kind": "bash",
                        "task_id": "task-parity",
                        "command": "cargo test",
                        "output": "PARITY_BACKGROUND_OUTPUT",
                        "exit_code": 0,
                        "truncated": false
                    },
                    "will_wake": true,
                }),
            ),
            update(
                1_777_000_025,
                id,
                json!({
                    "sessionUpdate": "turn_completed",
                    "prompt_id": "prompt-parity",
                    "stop_reason": "PARITY_END_TURN",
                }),
            ),
            update(
                1_777_000_026,
                id,
                json!({
                    "sessionUpdate": "memory_session_saved",
                    "path": "PARITY_MEMORY_PATH",
                }),
            ),
            update(
                1_777_000_027,
                id,
                json!({
                    "sessionUpdate": "compaction_checkpoint",
                    "checkpoint_file": "compaction_checkpoints/parity.json",
                    "checkpoint_id": "PARITY_CHECKPOINT_ID",
                }),
            ),
            update(
                1_777_000_028,
                id,
                json!({
                    "sessionUpdate": "auto_compact_completed",
                    "tokens_before": 1000,
                    "tokens_after": 400,
                    "summary_preview": "PARITY_COMPACTION_PREVIEW",
                }),
            ),
            update(
                1_777_000_029,
                id,
                json!({"sessionUpdate": "memory_flush_started"}),
            ),
            update(
                1_777_000_030,
                id,
                json!({
                    "sessionUpdate": "last_turn_summary",
                    "summary": "PARITY_TRANSIENT_TURN_SUMMARY",
                    "prompt_id": "prompt-parity",
                }),
            ),
            update(
                1_777_000_031,
                id,
                json!({
                    "sessionUpdate": "session_summary_generated",
                    "session_summary": "PARITY_GENERATED_TITLE",
                }),
            ),
            update(
                1_777_000_032,
                id,
                json!({
                    "sessionUpdate": "subagent_spawned",
                    "subagent_id": "child-parity",
                    "child_session_id": "child-parity",
                    "parent_session_id": id,
                    "subagent_type": "explore",
                    "description": "PARITY_SUBAGENT_PROMPT",
                }),
            ),
            update(
                1_777_000_033,
                id,
                json!({
                    "sessionUpdate": "scheduled_task_created",
                    "task_id": "scheduled-parity",
                    "prompt": "PARITY_SCHEDULED_PROMPT",
                    "human_schedule": "tomorrow",
                }),
            ),
            update(
                1_777_000_034,
                id,
                json!({
                    "sessionUpdate": "model_changed",
                    "model_id": "PARITY_CHANGED_MODEL",
                    "reasoning_effort": "high",
                }),
            ),
            update(
                1_777_000_035,
                id,
                json!({
                    "sessionUpdate": "image_dropped",
                    "notes": ["PARITY_DROPPED_IMAGE"],
                }),
            ),
            update(
                1_777_000_036,
                id,
                json!({
                    "sessionUpdate": "diff_review",
                    "content": [{
                        "path": "src/parity.rs",
                        "oldText": "old parity",
                        "newText": "PARITY_DIFF_REVIEW"
                    }],
                }),
            ),
        ],
    );
    fs::create_dir_all(session_dir.join("compaction_checkpoints")).unwrap();
    fs::write(
        session_dir.join("compaction_checkpoints/parity.json"),
        serde_json::to_vec(&json!({
            "compacted_history": [{
                "type": "user",
                "synthetic_reason": "compaction_meta",
                "content": [{
                    "type": "text",
                    "text": concat!(
                        "This session is being continued from a previous conversation that ran out of context. ",
                        "The summary below covers the earlier portion of the conversation.\n\n",
                        "PARITY_FULL_COMPACTION_SUMMARY"
                    )
                }]
            }]
        }))
        .unwrap(),
    )
    .unwrap();

    for (pattern, target) in [
        ("PARITY_PLAN_MARKER", "progress.plan"),
        ("PARITY_HOOK_OUTPUT", "progress.hook_execution"),
        ("PARITY_RETRY_REASON", "progress.retry_state"),
        ("PARITY_BACKGROUND_START", "progress.task_backgrounded"),
        ("PARITY_BACKGROUND_OUTPUT", "bash-output.background_task"),
        ("PARITY_END_TURN", "system.turn_completed"),
        ("PARITY_MEMORY_PATH", "progress.memory_session_saved"),
        ("PARITY_CHECKPOINT_ID", "progress.compaction_checkpoint"),
        (
            "PARITY_COMPACTION_PREVIEW",
            "progress.auto_compact_completed",
        ),
        ("PARITY_FULL_COMPACTION_SUMMARY", "compact-summary"),
        ("memory flush started", "progress.memory_flush_started"),
        ("PARITY_TRANSIENT_TURN_SUMMARY", "system.last_turn_summary"),
        ("PARITY_GENERATED_TITLE", "ai-title"),
        ("PARITY_SUBAGENT_PROMPT", "subagent-prompt"),
        (
            "PARITY_SCHEDULED_PROMPT",
            "queue-operation.scheduled_task_created",
        ),
        ("PARITY_CHANGED_MODEL", "mode"),
        ("PARITY_DROPPED_IMAGE", "attachment.image_dropped"),
        ("PARITY_DIFF_REVIEW", "tool-use.diff_review"),
    ] {
        let output = world
            .cmd()
            .args(["search", pattern, "-t", target])
            .output()
            .unwrap();
        assert!(
            output.status.success() && stdout(&output).contains(pattern),
            "[{target}] stdout: {} stderr: {}",
            stdout(&output),
            String::from_utf8_lossy(&output.stderr)
        );
    }
}

#[test]
fn formats_multimodal_content_and_structured_tool_results() {
    let world = GrokWorld::new();
    let id = "grok-content";
    world.add_session(
        id,
        "Content test",
        false,
        None,
        "2026-08-20T12:00:30Z",
        &[
            update(
                1_777_000_030,
                id,
                json!({
                    "sessionUpdate": "user_message_chunk",
                    "content": [
                        {"type": "text", "text": "PARITY_MULTIMODAL_PROMPT"},
                        {"type": "image", "mimeType": "image/png", "data": "aGVsbG8="},
                        {"type": "audio", "mimeType": "audio/wav", "data": "aGk="}
                    ],
                }),
            ),
            update(
                1_777_000_031,
                id,
                json!({
                    "sessionUpdate": "agent_message_chunk",
                    "content": [{
                        "type": "resource_link",
                        "name": "PARITY_RESOURCE_NAME",
                        "uri": "file:///repo/parity.txt",
                        "description": "PARITY_RESOURCE_DESCRIPTION"
                    }],
                }),
            ),
            update(
                1_777_000_032,
                id,
                json!({
                    "sessionUpdate": "tool_call",
                    "toolCallId": "call-structured",
                    "title": "search_replace",
                    "rawInput": {"file_path": "src/lib.rs", "old_string": "old", "new_string": "new"},
                    "_meta": {"x.ai/tool": {"name": "search_replace", "kind": "edit"}},
                }),
            ),
            update(
                1_777_000_033,
                id,
                json!({
                    "sessionUpdate": "tool_call_update",
                    "toolCallId": "call-structured",
                    "status": "failed",
                    "rawOutput": {"type": "SearchReplace", "NoMatchesFound": "PARITY_STRUCTURED_TOOL_ERROR"},
                }),
            ),
        ],
    );

    for (pattern, target, expected) in [
        ("image/png", "user", "[image: image/png, 5 bytes]"),
        (
            "PARITY_RESOURCE_NAME",
            "assistant",
            "file:///repo/parity.txt",
        ),
        (
            "PARITY_STRUCTURED_TOOL_ERROR",
            "tool-result.search_replace",
            "PARITY_STRUCTURED_TOOL_ERROR",
        ),
    ] {
        let output = world
            .cmd()
            .args(["search", pattern, "-t", target])
            .output()
            .unwrap();
        assert!(
            output.status.success() && stdout(&output).contains(expected),
            "[{target}] stdout: {} stderr: {}",
            stdout(&output),
            String::from_utf8_lossy(&output.stderr)
        );
    }
}

#[test]
fn maps_grok_summary_metadata() {
    let world = GrokWorld::new();
    world.add_session(
        "grok-metadata",
        "Metadata test",
        false,
        None,
        "2026-08-20T12:00:40Z",
        &[],
    );
    world.add_summary_fields(
        "grok-metadata",
        json!({
            "agent_name": "PARITY_AGENT_NAME",
            "current_model_id": "PARITY_MODEL_ID",
            "reasoning_effort": "PARITY_REASONING_EFFORT",
            "sandbox_profile": "PARITY_SANDBOX_PROFILE",
            "last_turn_summary": "PARITY_LAST_TURN_SUMMARY",
        }),
    );

    for (pattern, target) in [
        ("PARITY_AGENT_NAME", "agent-name"),
        ("PARITY_MODEL_ID", "system.model"),
        ("PARITY_REASONING_EFFORT", "system.reasoning_effort"),
        ("PARITY_SANDBOX_PROFILE", "permission-mode"),
        ("PARITY_LAST_TURN_SUMMARY", "system.last_turn_summary"),
    ] {
        let output = world
            .cmd()
            .args(["search", pattern, "-t", target])
            .output()
            .unwrap();
        assert!(
            output.status.success() && stdout(&output).contains(pattern),
            "[{target}] stdout: {} stderr: {}",
            stdout(&output),
            String::from_utf8_lossy(&output.stderr)
        );
    }
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
    fs::create_dir_all(world.home.path().join(".claude/rules")).unwrap();
    fs::write(
        world.home.path().join(".claude/CLAUDE.md"),
        "global claude compatibility marker\n",
    )
    .unwrap();
    fs::write(
        world.home.path().join(".claude/rules/global.md"),
        "global claude rules marker\n",
    )
    .unwrap();
    fs::create_dir_all(world.home.path().join(".cursor/rules")).unwrap();
    fs::write(
        world.home.path().join(".cursor/rules/global.md"),
        "global cursor rules marker\n",
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
    fs::write(
        world.project.path().join(".grok/rules/ignored.md"),
        "ignored grok rule marker\n",
    )
    .unwrap();
    fs::create_dir_all(world.project.path().join(".claude/rules")).unwrap();
    fs::write(
        world.project.path().join(".claude/CLAUDE.local.md"),
        "project claude compatibility marker\n",
    )
    .unwrap();
    fs::write(
        world.project.path().join(".claude/rules/project.md"),
        "project claude rules marker\n",
    )
    .unwrap();
    fs::create_dir_all(world.project.path().join(".cursor/rules")).unwrap();
    fs::write(
        world.project.path().join(".cursor/rules/project.md"),
        "project cursor rules marker\n",
    )
    .unwrap();
    fs::create_dir_all(world.project.path().join("src")).unwrap();
    fs::write(
        world.project.path().join("src/AGENTS.md"),
        "nested grok instruction marker\n",
    )
    .unwrap();
    fs::write(
        world.project.path().join(".gitignore"),
        ".grok/rules/ignored.md\n",
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
        "global claude compatibility",
        "global claude rules",
        "global cursor rules",
        "project claude compatibility",
        "project claude rules",
        "project cursor rules",
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

    let nested = world
        .cmd()
        .args(["memory", "search", "nested grok instruction"])
        .output()
        .unwrap();
    assert!(stdout(&nested).contains("nested grok instruction marker"));
    let nested_disabled = world
        .cmd()
        .args([
            "memory",
            "search",
            "nested grok instruction",
            "--no-subdirs",
        ])
        .output()
        .unwrap();
    assert!(!stdout(&nested_disabled).contains("nested grok instruction marker"));
    let ignored = world
        .cmd()
        .args(["memory", "search", "ignored grok rule"])
        .output()
        .unwrap();
    assert!(!stdout(&ignored).contains("ignored grok rule marker"));
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
fn reports_malformed_and_unknown_updates_without_losing_valid_records() {
    let world = GrokWorld::new();
    let session_dir = world.add_session(
        "grok-diagnostics",
        "Diagnostics test",
        false,
        None,
        "2026-08-20T12:00:50Z",
        &[update(
            1_777_000_050,
            "grok-diagnostics",
            json!({
                "sessionUpdate": "agent_message_chunk",
                "content": {"type": "text", "text": "PARITY_VALID_AFTER_DIAGNOSTIC"},
            }),
        )],
    );
    let mut file = fs::OpenOptions::new()
        .append(true)
        .open(session_dir.join("updates.jsonl"))
        .unwrap();
    writeln!(file, "{{not valid json").unwrap();
    for timestamp in [1_777_000_051, 1_777_000_052] {
        writeln!(
            file,
            "{}",
            update(
                timestamp,
                "grok-diagnostics",
                json!({
                    "sessionUpdate": "future_parity_event",
                    "payload": "PARITY_UNKNOWN_PAYLOAD",
                }),
            )
        )
        .unwrap();
    }
    drop(file);

    let output = world
        .cmd()
        .args(["dump", "grok-diagnostics", "-t", "all"])
        .output()
        .unwrap();
    assert!(output.status.success());
    assert!(stdout(&output).contains("PARITY_VALID_AFTER_DIAGNOSTIC"));
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("updates.jsonl: line 2"), "{stderr}");
    assert!(stderr.contains("future_parity_event"), "{stderr}");
    assert_eq!(
        stderr
            .matches("skipping unrecognized Grok session update")
            .count(),
        1,
        "{stderr}"
    );
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
    writeln!(file, "{{malformed completed update").unwrap();
    file.flush().unwrap();
    std::thread::sleep(Duration::from_millis(250));
    let appended = serde_json::to_string(&update(
        1_777_100_001,
        "grok-follow",
        json!({
            "sessionUpdate": "agent_message_chunk",
            "content": {"type": "text", "text": "GROK_FOLLOW_APPENDED"},
        }),
    ))
    .unwrap();
    let midpoint = appended.len() / 2;
    file.write_all(&appended.as_bytes()[..midpoint]).unwrap();
    file.flush().unwrap();
    std::thread::sleep(Duration::from_millis(300));
    file.write_all(&appended.as_bytes()[midpoint..]).unwrap();
    file.write_all(b"\n").unwrap();
    file.flush().unwrap();
    drop(file);
    std::thread::sleep(Duration::from_millis(500));

    child.kill().unwrap();
    let output = child.wait_with_output().unwrap();
    assert!(stdout(&output).contains("GROK_FOLLOW_APPENDED"));
    assert!(String::from_utf8_lossy(&output.stderr).contains("malformed appended Grok update"));
}
