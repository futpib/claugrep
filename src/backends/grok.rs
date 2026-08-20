//! Grok Build backend: reads durable ACP updates from
//! `$GROK_HOME/sessions/<encoded-cwd>/<session-id>/updates.jsonl`.

use std::collections::{HashMap, HashSet};
use std::fs;
use std::io::{BufRead, BufReader, Seek, SeekFrom};
use std::path::{Component, Path, PathBuf};
use std::process::Command;
use std::sync::OnceLock;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use ignore::gitignore::{Gitignore, GitignoreBuilder};
use serde_json::{json, Value};

use crate::memory::{MemoryFile, MemorySource};
use crate::parser::{EditDiff, ExtractedContent, Target};
use crate::sessions::{get_worktree_paths, ProjectInfo, SessionFile};
use crate::source::Source;

pub const GROK: &str = "grok";

pub struct GrokSource {
    pub home: PathBuf,
    sessions: OnceLock<Vec<GrokSessionMeta>>,
}

#[derive(Clone)]
struct GrokSessionMeta {
    id: String,
    cwd: String,
    updates_path: PathBuf,
    summary_path: PathBuf,
    mtime: SystemTime,
    is_subagent: bool,
}

#[derive(Clone)]
struct CallInfo {
    name: String,
    shell: bool,
}

#[derive(Default)]
struct ExtractState {
    calls: HashMap<String, CallInfo>,
    warned_update_kinds: HashSet<String>,
}

impl GrokSource {
    pub fn new(home: PathBuf) -> Self {
        Self {
            home,
            sessions: OnceLock::new(),
        }
    }

    pub fn default_home() -> PathBuf {
        std::env::var_os("GROK_HOME")
            .filter(|value| !value.is_empty())
            .map(PathBuf::from)
            .unwrap_or_else(|| dirs::home_dir().expect("no home dir").join(".grok"))
    }

    pub fn is_available(home: &Path) -> bool {
        home.join("sessions").is_dir()
            || home.join("memory").is_dir()
            || grok_instruction_names()
                .iter()
                .any(|name| home.join(name).is_file())
            || home.join("rules").is_dir()
    }

    fn sessions(&self) -> &[GrokSessionMeta] {
        self.sessions
            .get_or_init(|| discover_session_metadata(&self.home.join("sessions")))
    }
}

fn grok_instruction_names() -> [&'static str; 6] {
    [
        "Agents.md",
        "Claude.md",
        "CLAUDE.md",
        "CLAUDE.local.md",
        "AGENT.md",
        "AGENTS.md",
    ]
}

fn grok_project_instruction_names() -> [&'static str; 8] {
    [
        "Agents.md",
        "Claude.md",
        "CLAUDE.md",
        "CLAUDE.local.md",
        "AGENT.md",
        "AGENTS.md",
        ".claude/CLAUDE.md",
        ".claude/CLAUDE.local.md",
    ]
}

const GROK_PROJECT_RULE_DIRS: [&str; 3] = [".grok/rules", ".claude/rules", ".cursor/rules"];

struct InstructionDiscovery<'a> {
    matcher: Option<&'a Gitignore>,
    git_root: &'a Path,
    seen: &'a mut HashSet<PathBuf>,
    out: &'a mut Vec<MemoryFile>,
}

fn git_root(cwd: &Path) -> PathBuf {
    Command::new("git")
        .args(["rev-parse", "--show-toplevel"])
        .current_dir(cwd)
        .output()
        .ok()
        .filter(|output| output.status.success())
        .and_then(|output| {
            let path = String::from_utf8_lossy(&output.stdout).trim().to_string();
            (!path.is_empty()).then(|| PathBuf::from(path))
        })
        .unwrap_or_else(|| cwd.to_path_buf())
}

fn build_gitignore(root: &Path) -> Option<Gitignore> {
    let mut builder = GitignoreBuilder::new(root);
    let local = root.join(".gitignore");
    if local.is_file() {
        let _ = builder.add(local);
    }
    let global = Command::new("git")
        .args(["config", "--path", "--get", "core.excludesFile"])
        .current_dir(root)
        .output()
        .ok()
        .filter(|output| output.status.success())
        .and_then(|output| {
            let path = String::from_utf8_lossy(&output.stdout).trim().to_string();
            (!path.is_empty()).then(|| PathBuf::from(path))
        })
        .or_else(|| dirs::home_dir().map(|home| home.join(".gitignore")));
    if let Some(global) = global.filter(|path| path.is_file()) {
        let _ = builder.add(global);
    }
    builder.build().ok()
}

fn is_gitignored(path: &Path, matcher: Option<&Gitignore>, root: &Path) -> bool {
    let Some(matcher) = matcher else {
        return false;
    };
    let Ok(relative) = path.strip_prefix(root) else {
        return false;
    };
    matcher
        .matched_path_or_any_parents(relative, path.is_dir())
        .is_ignore()
}

fn direct_markdown(dir: &Path) -> Vec<PathBuf> {
    let Ok(entries) = fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut paths: Vec<_> = entries
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| {
            path.is_file()
                && path
                    .extension()
                    .and_then(|extension| extension.to_str())
                    .is_some_and(|extension| extension.eq_ignore_ascii_case("md"))
        })
        .collect();
    paths.sort();
    paths
}

fn add_instruction_root(
    dir: &Path,
    filenames: &[&str],
    rule_dirs: &[&str],
    source: MemorySource,
    discovery: &mut InstructionDiscovery<'_>,
) {
    let paths = filenames
        .iter()
        .map(|name| dir.join(name))
        .filter(|path| path.is_file())
        .chain(
            rule_dirs
                .iter()
                .flat_map(|rules| direct_markdown(&dir.join(rules))),
        );
    for path in paths {
        let canonical = path.canonicalize().unwrap_or_else(|_| path.clone());
        if !is_gitignored(&path, discovery.matcher, discovery.git_root)
            && discovery.seen.insert(canonical)
        {
            discovery.out.push(MemoryFile {
                path,
                source,
                imported_by: None,
            });
        }
    }
}

fn is_skipped_dir(name: &str) -> bool {
    matches!(
        name,
        ".git"
            | "node_modules"
            | "target"
            | "dist"
            | "build"
            | ".next"
            | ".nuxt"
            | ".cache"
            | ".venv"
            | "venv"
            | "__pycache__"
    )
}

fn collect_nested_instructions(dir: &Path, discovery: &mut InstructionDiscovery<'_>) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let Ok(file_type) = entry.file_type() else {
            continue;
        };
        if !file_type.is_dir() {
            continue;
        }
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if is_skipped_dir(&name) || is_gitignored(&path, discovery.matcher, discovery.git_root) {
            continue;
        }
        add_instruction_root(
            &path,
            &grok_project_instruction_names(),
            &GROK_PROJECT_RULE_DIRS,
            MemorySource::Subdir,
            discovery,
        );
        collect_nested_instructions(&path, discovery);
    }
}

fn discover_session_metadata(root: &Path) -> Vec<GrokSessionMeta> {
    let mut out = Vec::new();
    let Ok(project_dirs) = fs::read_dir(root) else {
        return out;
    };
    for project_dir in project_dirs.flatten() {
        let Ok(project_type) = project_dir.file_type() else {
            continue;
        };
        if !project_type.is_dir() {
            continue;
        }
        let Ok(session_dirs) = fs::read_dir(project_dir.path()) else {
            continue;
        };
        for session_dir in session_dirs.flatten() {
            let Ok(session_type) = session_dir.file_type() else {
                continue;
            };
            if !session_type.is_dir() {
                continue;
            }
            if let Some(meta) = read_session_meta(&session_dir.path()) {
                out.push(meta);
            }
        }
    }
    out
}

fn read_session_meta(dir: &Path) -> Option<GrokSessionMeta> {
    let summary_path = dir.join("summary.json");
    let updates_path = dir.join("updates.jsonl");
    if !summary_path.is_file() || !updates_path.is_file() {
        return None;
    }
    let summary: Value = serde_json::from_reader(fs::File::open(&summary_path).ok()?).ok()?;
    let id = summary["info"]["id"]
        .as_str()
        .or_else(|| dir.file_name().and_then(|name| name.to_str()))?
        .to_string();
    let cwd = summary["info"]["cwd"].as_str()?.to_string();
    let session_kind = summary["session_kind"].as_str().unwrap_or("");
    let is_subagent = session_kind.starts_with("subagent");
    let mtime = summary["updated_at"]
        .as_str()
        .and_then(parse_system_time)
        .or_else(|| fs::metadata(&summary_path).ok()?.modified().ok())
        .or_else(|| fs::metadata(&updates_path).ok()?.modified().ok())
        .unwrap_or(UNIX_EPOCH);
    Some(GrokSessionMeta {
        id,
        cwd,
        updates_path,
        summary_path,
        mtime,
        is_subagent,
    })
}

fn parse_system_time(value: &str) -> Option<SystemTime> {
    let timestamp = chrono::DateTime::parse_from_rfc3339(value)
        .ok()?
        .timestamp();
    if timestamp < 0 {
        None
    } else {
        Some(UNIX_EPOCH + Duration::from_secs(timestamp as u64))
    }
}

fn timestamp_iso(value: &Value) -> String {
    if let Some(value) = value.as_str() {
        return value.to_string();
    }
    let Some(value) = value.as_i64() else {
        return String::new();
    };
    if value > 10_000_000_000 {
        return chrono::DateTime::<chrono::Utc>::from_timestamp_millis(value)
            .map(|dt| dt.format("%Y-%m-%dT%H:%M:%S%.3fZ").to_string())
            .unwrap_or_default();
    }
    chrono::DateTime::<chrono::Utc>::from_timestamp(value, 0)
        .map(|dt| dt.format("%Y-%m-%dT%H:%M:%SZ").to_string())
        .unwrap_or_default()
}

fn normalized_raw(entry: &Value, target: &Target, session_id: &str, timestamp: &str) -> Value {
    let mut raw = entry.clone();
    if let Some(object) = raw.as_object_mut() {
        object.insert("sessionId".into(), Value::String(session_id.into()));
        object.insert(
            "itemType".into(),
            entry["params"]["update"]["sessionUpdate"].clone(),
        );
        object.insert("timestamp".into(), Value::String(timestamp.into()));
        object.insert("type".into(), Value::String(target.to_string()));
    }
    raw
}

fn extracted(
    target: Target,
    text: String,
    tool_name: Option<String>,
    timestamp: &str,
    session_id: &str,
    raw_entry: Option<Value>,
) -> ExtractedContent {
    ExtractedContent {
        target,
        text,
        tool_name,
        timestamp: timestamp.to_string(),
        session_id: session_id.to_string(),
        edit_diff: None,
        raw_entry,
    }
}

fn content_text(value: &Value) -> String {
    match value {
        Value::String(text) => text.clone(),
        Value::Array(items) => items
            .iter()
            .map(content_text)
            .filter(|text| !text.is_empty())
            .collect::<Vec<_>>()
            .join("\n"),
        Value::Object(object) => {
            if let Some(text) = object.get("text").and_then(Value::as_str) {
                return text.to_string();
            }
            let block_type = object.get("type").and_then(Value::as_str).unwrap_or("");
            match block_type {
                "image" => media_marker("image", value),
                "audio" => media_marker("audio", value),
                "resource_link" => resource_link_marker(value),
                "resource" => embedded_resource_text(value),
                "diff" => {
                    let path = value["path"].as_str().unwrap_or("file");
                    let old = value["oldText"].as_str().unwrap_or("");
                    let new = value["newText"].as_str().unwrap_or("");
                    format!("{}\n-{}\n+{}", path, old, new)
                }
                _ => object
                    .get("content")
                    .map(content_text)
                    .filter(|text| !text.is_empty())
                    .unwrap_or_default(),
            }
        }
        _ => String::new(),
    }
}

fn base64_decoded_len(data: &str) -> usize {
    let padding = data.bytes().rev().take_while(|byte| *byte == b'=').count();
    (data.len() / 4 * 3).saturating_sub(padding)
}

fn media_marker(kind: &str, value: &Value) -> String {
    let mime = value["mimeType"]
        .as_str()
        .or_else(|| value["mime_type"].as_str())
        .or_else(|| value["media_type"].as_str())
        .unwrap_or(kind);
    if let Some(uri) = value["uri"].as_str().filter(|uri| !uri.is_empty()) {
        return format!("[{}: {}, {}]", kind, mime, uri);
    }
    let bytes = value["data"].as_str().map(base64_decoded_len).unwrap_or(0);
    if bytes == 0 {
        format!("[{}: {}]", kind, mime)
    } else {
        format!("[{}: {}, {} bytes]", kind, mime, bytes)
    }
}

fn resource_link_marker(value: &Value) -> String {
    let name = value["name"]
        .as_str()
        .or_else(|| value["title"].as_str())
        .unwrap_or("resource");
    let uri = value["uri"].as_str().unwrap_or("");
    let description = value["description"].as_str().unwrap_or("");
    [
        if uri.is_empty() {
            format!("[resource: {}]", name)
        } else {
            format!("[resource: {}, {}]", name, uri)
        },
        description.to_string(),
    ]
    .into_iter()
    .filter(|part| !part.is_empty())
    .collect::<Vec<_>>()
    .join("\n")
}

fn embedded_resource_text(value: &Value) -> String {
    let resource = &value["resource"];
    let uri = resource["uri"].as_str().unwrap_or("");
    let text = resource["text"]
        .as_str()
        .map(str::to_owned)
        .or_else(|| {
            resource["blob"].as_str().map(|blob| {
                let mime = resource["mimeType"].as_str().unwrap_or("resource");
                format!(
                    "[resource data: {}, {} bytes]",
                    mime,
                    base64_decoded_len(blob)
                )
            })
        })
        .unwrap_or_default();
    match (uri.is_empty(), text.is_empty()) {
        (false, false) => format!("[resource: {}]\n{}", uri, text),
        (false, true) => format!("[resource: {}]", uri),
        (true, false) => text,
        (true, true) => "[resource]".to_string(),
    }
}

fn format_object_fields(value: &Value, excluded: &[&str]) -> String {
    let Some(object) = value.as_object() else {
        return content_text(value);
    };
    object
        .iter()
        .filter(|(key, value)| !excluded.contains(&key.as_str()) && !value.is_null())
        .map(|(key, value)| match value.as_str() {
            Some(text) if text.contains('\n') => format!("{}:\n{}", key, text),
            Some(text) => format!("{}: {}", key, text),
            None => format!("{}: {}", key, value),
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn format_update_fields(update: &Value, excluded: &[&str]) -> String {
    let text = format_object_fields(update, excluded);
    if text.is_empty() {
        update["sessionUpdate"]
            .as_str()
            .unwrap_or("update")
            .replace('_', " ")
    } else {
        text
    }
}

fn format_tool_input(input: &Value) -> String {
    format_object_fields(input, &["variant"])
}

fn tool_name(update: &Value) -> String {
    update["_meta"]["x.ai/tool"]["name"]
        .as_str()
        .or_else(|| update["title"].as_str())
        .unwrap_or("unknown")
        .trim_end_matches(':')
        .to_string()
}

fn is_shell_call(update: &Value, name: &str) -> bool {
    update["_meta"]["x.ai/tool"]["kind"] == "execute"
        || update["kind"] == "execute"
        || matches!(
            name.to_ascii_lowercase().as_str(),
            "run_terminal_command" | "bash" | "shell" | "exec" | "exec_command"
        )
}

fn edit_diff(name: &str, input: &Value) -> Option<EditDiff> {
    if !matches!(
        name.to_ascii_lowercase().as_str(),
        "edit" | "search_replace"
    ) {
        return None;
    }
    Some(EditDiff {
        file_path: input["file_path"].as_str()?.to_string(),
        old_string: input["old_string"].as_str()?.to_string(),
        new_string: input["new_string"].as_str()?.to_string(),
    })
}

fn tool_output(update: &Value) -> String {
    let from_content = content_text(&update["content"]);
    if !from_content.is_empty() {
        return from_content;
    }
    let raw = &update["rawOutput"];
    for path in [&raw["output_for_prompt"], &raw["output"], &raw["content"]] {
        if let Some(text) = path.as_str().filter(|text| !text.is_empty()) {
            return text.to_string();
        }
    }
    let stdout = raw["stdout"].as_str().unwrap_or("");
    let stderr = raw["stderr"].as_str().unwrap_or("");
    if !stdout.is_empty() || !stderr.is_empty() {
        return [stdout, stderr]
            .into_iter()
            .filter(|text| !text.is_empty())
            .collect::<Vec<_>>()
            .join("\n");
    }
    for key in [
        "Content",
        "FileContent",
        "Result",
        "EditsApplied",
        "TodosUpdated",
        "FileNotFound",
        "NotFound",
        "IsADirectory",
        "NoMatchesFound",
        "MultipleMatchesFound",
        "message",
        "error",
        "summary",
    ] {
        let text = content_text(&raw[key]);
        if !text.is_empty() {
            return text;
        }
        if !raw[key].is_null() {
            return match &raw[key] {
                Value::String(text) => text.clone(),
                value => format!("{}: {}", key, value),
            };
        }
    }
    format_object_fields(raw, &["type"])
}

#[allow(clippy::too_many_arguments)]
fn push_update_record(
    target: Target,
    subtype: Option<&str>,
    text: String,
    entry: &Value,
    timestamp: &str,
    session_id: &str,
    keep_raw: bool,
    out: &mut Vec<ExtractedContent>,
) {
    if text.is_empty() {
        return;
    }
    let raw = keep_raw.then(|| normalized_raw(entry, &target, session_id, timestamp));
    out.push(extracted(
        target,
        text,
        subtype.map(str::to_owned),
        timestamp,
        session_id,
        raw,
    ));
}

fn format_plan(update: &Value) -> String {
    update["entries"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|entry| {
            let content = entry["content"].as_str()?;
            let status = entry["status"].as_str().unwrap_or("pending");
            let priority = entry["priority"]
                .as_str()
                .map(|priority| format!("; {}", priority))
                .unwrap_or_default();
            Some(format!("[{}{}] {}", status, priority, content))
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn format_hook_execution(update: &Value) -> String {
    let event = update["event_name"].as_str().unwrap_or("hook");
    let tool = update["tool_name"]
        .as_str()
        .map(|tool| format!(" ({})", tool))
        .unwrap_or_default();
    let mut lines = vec![format!("{}{}", event, tool)];
    for run in update["runs"].as_array().into_iter().flatten() {
        let name = run["name"].as_str().unwrap_or("hook");
        let status = run["status"]["status"].as_str().unwrap_or("unknown");
        let elapsed = run["status"]["elapsed_ms"]
            .as_u64()
            .map(|elapsed| format!(" ({} ms)", elapsed))
            .unwrap_or_default();
        lines.push(format!("{}: {}{}", name, status, elapsed));
        if let Some(output) = run["output"].as_str().filter(|text| !text.is_empty()) {
            lines.push(output.to_string());
        }
        if let Some(error) = run["status"]["error"]
            .as_str()
            .filter(|text| !text.is_empty())
        {
            lines.push(error.to_string());
        }
    }
    lines.join("\n")
}

fn format_retry(update: &Value) -> String {
    let reason = update["reason"].as_str().unwrap_or("");
    match update["type"].as_str().unwrap_or("") {
        "retrying" => format!(
            "attempt {}/{}: {}",
            update["attempt"].as_u64().unwrap_or(0),
            update["max_retries"].as_u64().unwrap_or(0),
            reason
        ),
        "exhausted" => format!(
            "exhausted after {} attempts{}: {}",
            update["attempts"].as_u64().unwrap_or(0),
            if update["is_rate_limited"] == true {
                " (rate limited)"
            } else {
                ""
            },
            reason
        ),
        "failed" => format!(
            "{}: {}",
            update["error_type"].as_str().unwrap_or("failed"),
            update["message"].as_str().unwrap_or(reason)
        ),
        _ => format_object_fields(update, &["sessionUpdate"]),
    }
}

fn format_background_task(update: &Value) -> String {
    let description = update["description"].as_str().unwrap_or("");
    let command = update["command"].as_str().unwrap_or("");
    let task_id = update["task_id"].as_str().unwrap_or("");
    let cwd = update["cwd"].as_str().unwrap_or("");
    [
        description.to_string(),
        command.to_string(),
        if task_id.is_empty() {
            String::new()
        } else {
            format!("task: {}", task_id)
        },
        if cwd.is_empty() {
            String::new()
        } else {
            format!("cwd: {}", cwd)
        },
    ]
    .into_iter()
    .filter(|line| !line.is_empty())
    .collect::<Vec<_>>()
    .join("\n")
}

fn format_task_completion(snapshot: &Value) -> String {
    let output = snapshot["output"].as_str().unwrap_or("");
    let exit = snapshot["exit_code"]
        .as_i64()
        .map(|code| format!("exit code: {}", code))
        .unwrap_or_default();
    let signal = snapshot["signal"]
        .as_str()
        .map(|signal| format!("signal: {}", signal))
        .unwrap_or_default();
    let truncated = if snapshot["truncated"] == true {
        "output truncated".to_string()
    } else {
        String::new()
    };
    [output.to_string(), exit, signal, truncated]
        .into_iter()
        .filter(|line| !line.is_empty())
        .collect::<Vec<_>>()
        .join("\n")
}

fn checkpoint_summary(session_dir: &Path, update: &Value) -> Option<String> {
    let relative = Path::new(update["checkpoint_file"].as_str()?);
    if relative.is_absolute()
        || relative
            .components()
            .any(|component| !matches!(component, Component::Normal(_)))
    {
        return None;
    }
    let session_dir = session_dir.canonicalize().ok()?;
    let checkpoint_path = session_dir.join(relative).canonicalize().ok()?;
    if !checkpoint_path.starts_with(&session_dir) {
        return None;
    }
    let checkpoint: Value = serde_json::from_reader(fs::File::open(checkpoint_path).ok()?).ok()?;
    let history = checkpoint["compacted_history"].as_array()?;
    let continuation = "This session is being continued from a previous conversation that ran out of context. The summary below covers the earlier portion of the conversation.";
    history.iter().rev().find_map(|item| {
        if item["synthetic_reason"] != "compaction_meta" {
            return None;
        }
        let text = content_text(&item["content"]);
        let summary = text.strip_prefix(continuation).unwrap_or(&text).trim();
        (!summary.is_empty()).then(|| summary.to_string())
    })
}

#[allow(clippy::too_many_arguments)]
fn extract_entry(
    entry: &Value,
    source_path: &Path,
    session_dir: &Path,
    targets: &HashSet<Target>,
    session_id: &str,
    is_subagent: bool,
    keep_raw: bool,
    state: &mut ExtractState,
    out: &mut Vec<ExtractedContent>,
) {
    let update = &entry["params"]["update"];
    let kind = update["sessionUpdate"].as_str().unwrap_or("");
    let timestamp = timestamp_iso(&entry["timestamp"]);
    let raw =
        |target: &Target| keep_raw.then(|| normalized_raw(entry, target, session_id, &timestamp));

    match kind {
        "user_message_chunk" => {
            let target = if is_subagent {
                Target::SubagentPrompt
            } else {
                Target::User
            };
            if targets.contains(&target) {
                let text = content_text(&update["content"]);
                if !text.is_empty() {
                    out.push(extracted(
                        target.clone(),
                        text,
                        None,
                        &timestamp,
                        session_id,
                        raw(&target),
                    ));
                }
            }
        }
        "agent_message_chunk" => {
            if targets.contains(&Target::Assistant) {
                let text = content_text(&update["content"]);
                if !text.is_empty() {
                    out.push(extracted(
                        Target::Assistant,
                        text,
                        None,
                        &timestamp,
                        session_id,
                        raw(&Target::Assistant),
                    ));
                }
            }
        }
        "agent_thought_chunk" => {
            if targets.contains(&Target::Thinking) {
                let text = content_text(&update["content"]);
                if !text.is_empty() {
                    out.push(extracted(
                        Target::Thinking,
                        text,
                        None,
                        &timestamp,
                        session_id,
                        raw(&Target::Thinking),
                    ));
                }
            }
        }
        "tool_call" => {
            let name = tool_name(update);
            let shell = is_shell_call(update, &name);
            if let Some(id) = update["toolCallId"].as_str() {
                state.calls.insert(
                    id.to_string(),
                    CallInfo {
                        name: name.clone(),
                        shell,
                    },
                );
            }
            let input = &update["rawInput"];
            if targets.contains(&Target::ToolUse) {
                let mut record = extracted(
                    Target::ToolUse,
                    format_tool_input(input),
                    Some(name.clone()),
                    &timestamp,
                    session_id,
                    raw(&Target::ToolUse),
                );
                record.edit_diff = edit_diff(&name, input);
                out.push(record);
            }
            if shell && targets.contains(&Target::BashCommand) {
                if let Some(command) = input["command"].as_str().filter(|text| !text.is_empty()) {
                    out.push(extracted(
                        Target::BashCommand,
                        command.to_string(),
                        Some(name),
                        &timestamp,
                        session_id,
                        raw(&Target::BashCommand),
                    ));
                }
            }
        }
        "tool_call_update" if matches!(update["status"].as_str(), Some("completed" | "failed")) => {
            let Some(id) = update["toolCallId"].as_str() else {
                return;
            };
            let call = state.calls.get(id).cloned().unwrap_or_else(|| CallInfo {
                name: tool_name(update),
                shell: is_shell_call(update, &tool_name(update)),
            });
            let target = if call.shell {
                Target::BashOutput
            } else {
                Target::ToolResult
            };
            if targets.contains(&target) {
                let text = tool_output(update);
                if !text.is_empty() {
                    out.push(extracted(
                        target.clone(),
                        text,
                        Some(call.name),
                        &timestamp,
                        session_id,
                        raw(&target),
                    ));
                }
            }
        }
        "tool_call_update" => {}
        "session_recap" => {
            if targets.contains(&Target::CompactSummary) {
                if let Some(text) = update["summary"].as_str().filter(|text| !text.is_empty()) {
                    push_update_record(
                        Target::CompactSummary,
                        None,
                        text.to_string(),
                        entry,
                        &timestamp,
                        session_id,
                        keep_raw,
                        out,
                    );
                }
            }
        }
        "plan" => {
            if targets.contains(&Target::Progress) {
                push_update_record(
                    Target::Progress,
                    Some("plan"),
                    format_plan(update),
                    entry,
                    &timestamp,
                    session_id,
                    keep_raw,
                    out,
                );
            }
        }
        "hook_execution" => {
            if targets.contains(&Target::Progress) {
                push_update_record(
                    Target::Progress,
                    Some("hook_execution"),
                    format_hook_execution(update),
                    entry,
                    &timestamp,
                    session_id,
                    keep_raw,
                    out,
                );
            }
        }
        "retry_state" => {
            if targets.contains(&Target::Progress) {
                push_update_record(
                    Target::Progress,
                    Some("retry_state"),
                    format_retry(update),
                    entry,
                    &timestamp,
                    session_id,
                    keep_raw,
                    out,
                );
            }
        }
        "task_backgrounded" => {
            if targets.contains(&Target::Progress) {
                push_update_record(
                    Target::Progress,
                    Some("task_backgrounded"),
                    format_background_task(update),
                    entry,
                    &timestamp,
                    session_id,
                    keep_raw,
                    out,
                );
            }
        }
        "task_completed" => {
            let snapshot = &update["task_snapshot"];
            let task_kind = snapshot["kind"].as_str().unwrap_or("background_task");
            let result_target = if task_kind == "bash" {
                Target::BashOutput
            } else {
                Target::ToolResult
            };
            if targets.contains(&result_target) {
                push_update_record(
                    result_target,
                    Some("background_task"),
                    format_task_completion(snapshot),
                    entry,
                    &timestamp,
                    session_id,
                    keep_raw,
                    out,
                );
            }
            if targets.contains(&Target::Progress) {
                push_update_record(
                    Target::Progress,
                    Some("task_completed"),
                    format_object_fields(snapshot, &["output"]),
                    entry,
                    &timestamp,
                    session_id,
                    keep_raw,
                    out,
                );
            }
        }
        "turn_completed" => {
            if targets.contains(&Target::System) {
                push_update_record(
                    Target::System,
                    Some("turn_completed"),
                    format_object_fields(update, &["sessionUpdate"]),
                    entry,
                    &timestamp,
                    session_id,
                    keep_raw,
                    out,
                );
            }
        }
        "compaction_checkpoint" => {
            if targets.contains(&Target::CompactSummary) {
                if let Some(summary) = checkpoint_summary(session_dir, update) {
                    push_update_record(
                        Target::CompactSummary,
                        None,
                        summary,
                        entry,
                        &timestamp,
                        session_id,
                        keep_raw,
                        out,
                    );
                }
            }
            if targets.contains(&Target::Progress) {
                push_update_record(
                    Target::Progress,
                    Some("compaction_checkpoint"),
                    format_object_fields(update, &["sessionUpdate"]),
                    entry,
                    &timestamp,
                    session_id,
                    keep_raw,
                    out,
                );
            }
        }
        "auto_compact_completed" => {
            if targets.contains(&Target::Progress) {
                push_update_record(
                    Target::Progress,
                    Some("auto_compact_completed"),
                    format_object_fields(update, &["sessionUpdate"]),
                    entry,
                    &timestamp,
                    session_id,
                    keep_raw,
                    out,
                );
            }
        }
        "memory_session_saved" => {
            if targets.contains(&Target::Progress) {
                push_update_record(
                    Target::Progress,
                    Some("memory_session_saved"),
                    format_object_fields(update, &["sessionUpdate"]),
                    entry,
                    &timestamp,
                    session_id,
                    keep_raw,
                    out,
                );
            }
        }
        "session_summary_generated" => {
            if targets.contains(&Target::AiTitle) {
                push_update_record(
                    Target::AiTitle,
                    None,
                    update["session_summary"].as_str().unwrap_or("").to_string(),
                    entry,
                    &timestamp,
                    session_id,
                    keep_raw,
                    out,
                );
            }
        }
        "subagent_spawned" => {
            if targets.contains(&Target::SubagentPrompt) {
                push_update_record(
                    Target::SubagentPrompt,
                    None,
                    update["description"].as_str().unwrap_or("").to_string(),
                    entry,
                    &timestamp,
                    session_id,
                    keep_raw,
                    out,
                );
            }
            if targets.contains(&Target::Progress) {
                push_update_record(
                    Target::Progress,
                    Some("subagent_spawned"),
                    format_object_fields(update, &["sessionUpdate"]),
                    entry,
                    &timestamp,
                    session_id,
                    keep_raw,
                    out,
                );
            }
        }
        "diff_review" => {
            if targets.contains(&Target::ToolUse) {
                for diff in update["content"].as_array().into_iter().flatten() {
                    let mut record = extracted(
                        Target::ToolUse,
                        format_object_fields(diff, &[]),
                        Some("diff_review".to_string()),
                        &timestamp,
                        session_id,
                        raw(&Target::ToolUse),
                    );
                    record.edit_diff = Some(EditDiff {
                        file_path: diff["path"].as_str().unwrap_or("file").to_string(),
                        old_string: diff["oldText"].as_str().unwrap_or("").to_string(),
                        new_string: diff["newText"].as_str().unwrap_or("").to_string(),
                    });
                    out.push(record);
                }
            }
        }
        "scheduled_task_created" | "scheduled_task_fired" | "scheduled_task_deleted" => {
            if targets.contains(&Target::QueueOperation) {
                push_update_record(
                    Target::QueueOperation,
                    Some(kind),
                    format_object_fields(update, &["sessionUpdate"]),
                    entry,
                    &timestamp,
                    session_id,
                    keep_raw,
                    out,
                );
            }
        }
        "model_auto_switched" | "model_changed" | "current_mode_update" => {
            if targets.contains(&Target::Mode) {
                push_update_record(
                    Target::Mode,
                    None,
                    format_object_fields(update, &["sessionUpdate"]),
                    entry,
                    &timestamp,
                    session_id,
                    keep_raw,
                    out,
                );
            }
        }
        "image_compressed" | "image_dropped" => {
            if targets.contains(&Target::Attachment) {
                push_update_record(
                    Target::Attachment,
                    Some(kind),
                    format_object_fields(update, &["sessionUpdate", "images"]),
                    entry,
                    &timestamp,
                    session_id,
                    keep_raw,
                    out,
                );
            }
        }
        "feedback_request"
        | "rewind_marker"
        | "pending_interaction"
        | "interaction_resolved"
        | "last_turn_summary"
        | "response_started"
        | "response_completed" => {
            if targets.contains(&Target::System) {
                push_update_record(
                    Target::System,
                    Some(kind),
                    format_update_fields(update, &["sessionUpdate", "signature"]),
                    entry,
                    &timestamp,
                    session_id,
                    keep_raw,
                    out,
                );
            }
        }
        "auto_compact_started"
        | "auto_compact_failed"
        | "memory_flush_started"
        | "memory_flush_completed"
        | "memory_dream_completed"
        | "auto_compact_cancelled"
        | "auto_continue_completed"
        | "relay_sync_status"
        | "auto_recovery_started"
        | "auto_recovery_exhausted"
        | "hook_annotation"
        | "hooks_changed"
        | "plugins_changed"
        | "plugin_updates_installed"
        | "session_recap_unavailable"
        | "subagent_progress"
        | "subagent_finished"
        | "monitor_event"
        | "memory_files"
        | "workflow_updated"
        | "goal_updated"
        | "available_commands_update" => {
            if targets.contains(&Target::Progress) {
                push_update_record(
                    Target::Progress,
                    Some(kind),
                    format_update_fields(update, &["sessionUpdate"]),
                    entry,
                    &timestamp,
                    session_id,
                    keep_raw,
                    out,
                );
            }
        }
        "reasoning_completed" | "tool_call_delta_chunk" => {}
        _ => {
            let warning_key = if kind.is_empty() { "<missing>" } else { kind };
            if state.warned_update_kinds.insert(warning_key.to_string()) {
                let serialized = update.to_string();
                let preview: String = serialized.chars().take(120).collect();
                let ellipsis = if serialized.chars().count() > 120 {
                    "..."
                } else {
                    ""
                };
                eprintln!(
                    "warning: {}: skipping unrecognized Grok session update '{}': {}{}",
                    source_path.display(),
                    warning_key,
                    preview,
                    ellipsis
                );
            }
        }
    }
}

fn summary_title(path: &Path) -> Option<(String, bool, String)> {
    let summary: Value = serde_json::from_reader(fs::File::open(path).ok()?).ok()?;
    let title = summary["generated_title"]
        .as_str()
        .filter(|title| !title.trim().is_empty())
        .or_else(|| {
            summary["session_summary"]
                .as_str()
                .filter(|title| !title.trim().is_empty())
        })?
        .to_string();
    let manual = summary["title_is_manual"].as_bool().unwrap_or(false);
    let timestamp = summary["updated_at"].as_str().unwrap_or("").to_string();
    Some((title, manual, timestamp))
}

#[allow(clippy::too_many_arguments)]
fn push_summary_record(
    target: Target,
    subtype: Option<&str>,
    item_type: &str,
    text: &str,
    timestamp: &str,
    session_id: &str,
    keep_raw: bool,
    out: &mut Vec<ExtractedContent>,
) {
    if text.is_empty() {
        return;
    }
    let raw = keep_raw.then(|| {
        json!({
            "sessionId": session_id,
            "timestamp": timestamp,
            "type": target.to_string(),
            "itemType": item_type,
            "value": text,
        })
    });
    out.push(extracted(
        target,
        text.to_string(),
        subtype.map(str::to_owned),
        timestamp,
        session_id,
        raw,
    ));
}

fn append_summary_records(
    path: &Path,
    targets: &HashSet<Target>,
    session_id: &str,
    keep_raw: bool,
    out: &mut Vec<ExtractedContent>,
) {
    let Ok(file) = fs::File::open(path) else {
        return;
    };
    let Ok(summary) = serde_json::from_reader::<_, Value>(file) else {
        return;
    };
    let timestamp = summary["updated_at"].as_str().unwrap_or("");
    if let Some((title, manual, _)) = summary_title(path) {
        let target = if manual {
            Target::CustomTitle
        } else {
            Target::AiTitle
        };
        if targets.contains(&target) {
            push_summary_record(
                target,
                None,
                "summary-title",
                &title,
                timestamp,
                session_id,
                keep_raw,
                out,
            );
        }
    }
    if targets.contains(&Target::AgentName) {
        push_summary_record(
            Target::AgentName,
            None,
            "summary-agent-name",
            summary["agent_name"].as_str().unwrap_or(""),
            timestamp,
            session_id,
            keep_raw,
            out,
        );
    }
    if targets.contains(&Target::PermissionMode) {
        push_summary_record(
            Target::PermissionMode,
            None,
            "summary-sandbox-profile",
            summary["sandbox_profile"].as_str().unwrap_or(""),
            timestamp,
            session_id,
            keep_raw,
            out,
        );
    }
    if targets.contains(&Target::System) {
        for (subtype, field) in [
            ("model", "current_model_id"),
            ("reasoning_effort", "reasoning_effort"),
            ("last_turn_summary", "last_turn_summary"),
        ] {
            push_summary_record(
                Target::System,
                Some(subtype),
                &format!("summary-{}", field.replace('_', "-")),
                summary[field].as_str().unwrap_or(""),
                timestamp,
                session_id,
                keep_raw,
                out,
            );
        }
    }
}

fn dedupe_titles(out: &mut Vec<ExtractedContent>) {
    let mut seen = HashSet::new();
    out.retain(|record| {
        !matches!(record.target, Target::CustomTitle | Target::AiTitle)
            || seen.insert((record.target.clone(), record.text.clone()))
    });
}

fn collect_markdown(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let Ok(file_type) = entry.file_type() else {
            continue;
        };
        if file_type.is_dir() {
            collect_markdown(&path, out);
        } else if file_type.is_file()
            && path.extension().and_then(|extension| extension.to_str()) == Some("md")
        {
            out.push(path);
        }
    }
}

fn normalize_remote_url(url: &str) -> Option<String> {
    let colon = url.find(':')?;
    let path = {
        if url[..colon].contains('@') && !url[..colon].contains('/') {
            &url[colon + 1..]
        } else {
            url.split("//")
                .nth(1)
                .and_then(|rest| rest.split_once('/'))
                .map(|(_, path)| path)?
        }
    };
    let cleaned = path
        .trim_end_matches(".git")
        .trim_end_matches('/')
        .trim_start_matches('/');
    (!cleaned.is_empty() && cleaned.contains('/')).then(|| cleaned.to_string())
}

fn slugify(input: &str, max_len: usize) -> String {
    let mut out = String::new();
    let mut previous_dash = false;
    for character in input.to_lowercase().chars() {
        let character = if character.is_ascii_alphanumeric() {
            character
        } else {
            '-'
        };
        if character == '-' {
            if !previous_dash {
                out.push(character);
            }
            previous_dash = true;
        } else {
            out.push(character);
            previous_dash = false;
        }
    }
    out.chars()
        .take(max_len)
        .collect::<String>()
        .trim_matches('-')
        .to_string()
}

fn workspace_memory_dir(memory_root: &Path, cwd: &Path) -> PathBuf {
    let remote = Command::new("git")
        .args(["config", "--get", "remote.origin.url"])
        .current_dir(cwd)
        .output()
        .ok()
        .filter(|output| output.status.success())
        .and_then(|output| normalize_remote_url(String::from_utf8_lossy(&output.stdout).trim()));
    let (slug, identity) = match remote {
        Some(identity) => {
            let leaf = identity.rsplit('/').next().unwrap_or(&identity);
            (slugify(leaf, 40), identity)
        }
        None => {
            let canonical = cwd.canonicalize().unwrap_or_else(|_| cwd.to_path_buf());
            let leaf = canonical
                .file_name()
                .and_then(|name| name.to_str())
                .unwrap_or("workspace");
            (slugify(leaf, 40), canonical.to_string_lossy().to_string())
        }
    };
    let slug = if slug.is_empty() { "workspace" } else { &slug };
    let hash = blake3::hash(identity.as_bytes());
    memory_root.join(format!("{}-{}", slug, &hash.to_hex()[..8]))
}

impl Source for GrokSource {
    fn name(&self) -> &'static str {
        GROK
    }

    fn discover_projects(&self) -> Vec<ProjectInfo> {
        let mut grouped: HashMap<String, (usize, SystemTime)> = HashMap::new();
        for session in self.sessions() {
            let entry = grouped
                .entry(session.cwd.clone())
                .or_insert((0, UNIX_EPOCH));
            entry.0 += 1;
            entry.1 = entry.1.max(session.mtime);
        }
        let mut out: Vec<_> = grouped
            .into_iter()
            .map(|(cwd, (session_count, latest))| ProjectInfo {
                encoded_path: cwd.clone(),
                decoded_path: cwd.clone(),
                verified: Path::new(&cwd).exists(),
                session_count,
                latest_mtime: Some(latest),
                account: None,
                backend: GROK,
            })
            .collect();
        out.sort_by_key(|project| std::cmp::Reverse(project.latest_mtime));
        out
    }

    fn discover_sessions(&self, project_path: &str) -> Vec<SessionFile> {
        let mut paths: HashSet<String> = get_worktree_paths(project_path).into_iter().collect();
        paths.insert(project_path.to_string());
        let mut out: Vec<_> = self
            .sessions()
            .iter()
            .filter(|session| paths.contains(&session.cwd))
            .map(|session| SessionFile {
                session_id: session.id.clone(),
                file_path: session.updates_path.clone(),
                mtime: session.mtime,
                is_subagent: session.is_subagent,
                backend: GROK,
            })
            .collect();
        out.sort_by_key(|session| std::cmp::Reverse(session.mtime));
        out
    }

    fn extract_content(
        &self,
        session: &SessionFile,
        targets: &HashSet<Target>,
        keep_raw: bool,
    ) -> Vec<ExtractedContent> {
        let Ok(file) = fs::File::open(&session.file_path) else {
            return Vec::new();
        };
        let session_dir = session.file_path.parent().unwrap_or_else(|| Path::new("."));
        let mut out = Vec::new();
        let mut state = ExtractState::default();
        for (line_number, line) in BufReader::new(file).lines().enumerate() {
            let Ok(line) = line else {
                continue;
            };
            match serde_json::from_str(&line) {
                Ok(entry) => extract_entry(
                    &entry,
                    &session.file_path,
                    session_dir,
                    targets,
                    &session.session_id,
                    session.is_subagent,
                    keep_raw,
                    &mut state,
                    &mut out,
                ),
                Err(error) => eprintln!(
                    "warning: {}: line {}: {}",
                    session.file_path.display(),
                    line_number + 1,
                    error
                ),
            }
        }
        if let Some(summary_path) = session
            .file_path
            .parent()
            .map(|dir| dir.join("summary.json"))
        {
            append_summary_records(
                &summary_path,
                targets,
                &session.session_id,
                keep_raw,
                &mut out,
            );
        }
        dedupe_titles(&mut out);
        out
    }

    fn follow(
        &self,
        session: &SessionFile,
        targets: &HashSet<Target>,
        on_records: &mut dyn FnMut(&[ExtractedContent]),
    ) -> Result<(), String> {
        let mut state = ExtractState::default();
        let session_dir = session.file_path.parent().unwrap_or_else(|| Path::new("."));
        if let Ok(seed) = fs::File::open(&session.file_path) {
            let none = HashSet::new();
            let mut ignored = Vec::new();
            for line in BufReader::new(seed).lines().map_while(Result::ok) {
                if let Ok(entry) = serde_json::from_str(&line) {
                    extract_entry(
                        &entry,
                        &session.file_path,
                        session_dir,
                        &none,
                        &session.session_id,
                        session.is_subagent,
                        false,
                        &mut state,
                        &mut ignored,
                    );
                }
            }
        }
        let mut file = fs::File::open(&session.file_path).map_err(|error| error.to_string())?;
        file.seek(SeekFrom::End(0))
            .map_err(|error| error.to_string())?;
        let mut reader = BufReader::new(file);
        loop {
            let mut line = String::new();
            match reader.read_line(&mut line) {
                Ok(0) => std::thread::sleep(Duration::from_millis(200)),
                Ok(_) => match serde_json::from_str(&line) {
                    Ok(entry) => {
                        let mut out = Vec::new();
                        extract_entry(
                            &entry,
                            &session.file_path,
                            session_dir,
                            targets,
                            &session.session_id,
                            session.is_subagent,
                            false,
                            &mut state,
                            &mut out,
                        );
                        if !out.is_empty() {
                            on_records(&out);
                        }
                    }
                    Err(error) if line.ends_with('\n') => {
                        eprintln!(
                            "warning: {}: malformed appended Grok update: {}",
                            session.file_path.display(),
                            error
                        );
                    }
                    Err(_) => {
                        let bytes = line.len() as i64;
                        let inner = reader.get_mut();
                        let _ = inner.seek(SeekFrom::Current(-bytes));
                        reader = BufReader::new(reader.into_inner());
                        std::thread::sleep(Duration::from_millis(200));
                    }
                },
                Err(_) => std::thread::sleep(Duration::from_millis(200)),
            }
        }
    }

    fn discover_memory_files(&self, cwd: &Path, include_subdirs: bool) -> Vec<MemoryFile> {
        let root = git_root(cwd);
        let matcher = build_gitignore(&root);
        let mut out = Vec::new();
        let mut seen = HashSet::new();

        {
            let mut discovery = InstructionDiscovery {
                matcher: None,
                git_root: &root,
                seen: &mut seen,
                out: &mut out,
            };
            add_instruction_root(
                &self.home,
                &grok_instruction_names(),
                &["rules"],
                MemorySource::UserGlobal,
                &mut discovery,
            );
            if let Some(home) = dirs::home_dir() {
                for vendor in [".claude", ".cursor"] {
                    add_instruction_root(
                        &home.join(vendor),
                        &grok_instruction_names(),
                        &["rules"],
                        MemorySource::UserGlobal,
                        &mut discovery,
                    );
                }
            }
        }

        let mut chain = Vec::new();
        let mut current = Some(cwd);
        while let Some(dir) = current {
            chain.push(dir);
            if dir == root {
                break;
            }
            current = dir.parent();
        }
        chain.reverse();
        {
            let mut discovery = InstructionDiscovery {
                matcher: matcher.as_ref(),
                git_root: &root,
                seen: &mut seen,
                out: &mut out,
            };
            for dir in chain {
                add_instruction_root(
                    dir,
                    &grok_project_instruction_names(),
                    &GROK_PROJECT_RULE_DIRS,
                    MemorySource::Ancestor,
                    &mut discovery,
                );
            }
            if include_subdirs {
                collect_nested_instructions(cwd, &mut discovery);
            }
        }

        let memory_root = self.home.join("memory");
        let global = memory_root.join("MEMORY.md");
        let canonical_global = global.canonicalize().unwrap_or_else(|_| global.clone());
        if global.is_file() && seen.insert(canonical_global) {
            out.push(MemoryFile {
                path: global,
                source: MemorySource::GrokMemoryGlobal,
                imported_by: None,
            });
        }
        let workspace = workspace_memory_dir(&memory_root, cwd);
        let mut paths = Vec::new();
        collect_markdown(&workspace, &mut paths);
        paths.sort();
        for path in paths {
            let canonical = path.canonicalize().unwrap_or_else(|_| path.clone());
            if seen.insert(canonical) {
                out.push(MemoryFile {
                    path,
                    source: MemorySource::GrokMemoryWorkspace,
                    imported_by: None,
                });
            }
        }
        out
    }

    fn session_title(&self, session: &SessionFile) -> Option<String> {
        let summary_path = self
            .sessions()
            .iter()
            .find(|meta| meta.id == session.session_id && meta.updates_path == session.file_path)
            .map(|meta| meta.summary_path.clone())
            .or_else(|| {
                session
                    .file_path
                    .parent()
                    .map(|dir| dir.join("summary.json"))
            })?;
        summary_title(&summary_path).map(|(title, _, _)| title)
    }
}
