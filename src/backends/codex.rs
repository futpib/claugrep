//! Codex backend: reads JSONL rollout files from `$CODEX_HOME/sessions`.

use std::collections::{HashMap, HashSet};
use std::fs;
use std::io::{BufRead, BufReader, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use serde_json::Value;

use crate::memory::{discover_with_layout, MemoryFile, MemoryLayout, MemorySource};
use crate::parser::{EditDiff, ExtractedContent, Target};
use crate::sessions::{get_worktree_paths, ProjectInfo, SessionFile};
use crate::source::Source;

pub const CODEX: &str = "codex";

pub struct CodexSource {
    pub home: PathBuf,
    rollouts: OnceLock<Vec<RolloutMeta>>,
}

#[derive(Clone)]
struct RolloutMeta {
    id: String,
    cwd: String,
    path: PathBuf,
    mtime: std::time::SystemTime,
    is_subagent: bool,
}

impl CodexSource {
    pub fn new(home: PathBuf) -> Self {
        Self {
            home,
            rollouts: OnceLock::new(),
        }
    }

    pub fn default_home() -> PathBuf {
        std::env::var("CODEX_HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|_| dirs::home_dir().expect("no home dir").join(".codex"))
    }

    pub fn is_available(home: &Path) -> bool {
        home.join("sessions").is_dir()
            || home.join("memories").is_dir()
            || home.join("AGENTS.md").is_file()
    }

    fn rollouts(&self) -> &[RolloutMeta] {
        self.rollouts.get_or_init(|| {
            let mut files = Vec::new();
            collect_jsonl(&self.home.join("sessions"), &mut files);
            files.into_iter().filter_map(read_meta).collect()
        })
    }
}

fn collect_jsonl(dir: &Path, out: &mut Vec<PathBuf>) {
    let entries = match fs::read_dir(dir) {
        Ok(v) => v,
        Err(_) => return,
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            collect_jsonl(&path, out);
        } else if path.extension().and_then(|x| x.to_str()) == Some("jsonl") {
            out.push(path);
        }
    }
}

/// Discover Codex's first-class memory store. The summary and registry are
/// emitted first because they are the entry points Codex itself uses; remaining
/// markdown resources follow in stable path order for deterministic output.
fn discover_native_memory_files(home: &Path) -> Vec<MemoryFile> {
    let root = home.join("memories");
    let summary = root.join("memory_summary.md");
    let registry = root.join("MEMORY.md");
    let transient_diff = root.join("phase2_workspace_diff.md");
    let mut paths = Vec::new();

    collect_native_memory_markdown(&root, &transient_diff, &mut paths);
    paths.sort_by(|a, b| {
        let rank = |path: &Path| {
            if path == summary {
                0
            } else if path == registry {
                1
            } else {
                2
            }
        };
        rank(a).cmp(&rank(b)).then_with(|| a.cmp(b))
    });

    paths
        .into_iter()
        .map(|path| {
            let source = if path == summary {
                MemorySource::CodexMemorySummary
            } else if path == registry {
                MemorySource::CodexMemoryRegistry
            } else {
                MemorySource::CodexMemoryFile
            };
            MemoryFile {
                path,
                source,
                imported_by: None,
            }
        })
        .collect()
}

fn collect_native_memory_markdown(dir: &Path, transient_diff: &Path, out: &mut Vec<PathBuf>) {
    let entries = match fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(_) => return,
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let Ok(file_type) = entry.file_type() else {
            continue;
        };
        if file_type.is_dir() {
            if entry.file_name() != ".git" {
                collect_native_memory_markdown(&path, transient_diff, out);
            }
        } else if file_type.is_file()
            && path != transient_diff
            && path.extension().and_then(|extension| extension.to_str()) == Some("md")
        {
            out.push(path);
        }
    }
}

fn read_meta(path: PathBuf) -> Option<RolloutMeta> {
    let file = fs::File::open(&path).ok()?;
    for line in BufReader::new(file).lines().take(20).flatten() {
        let value: Value = serde_json::from_str(&line).ok()?;
        if value["type"] != "session_meta" {
            continue;
        }
        let payload = &value["payload"];
        let id = payload["id"]
            .as_str()
            .or_else(|| payload["session_id"].as_str())?
            .to_string();
        let cwd = payload["cwd"].as_str()?.to_string();
        let is_subagent = match &payload["thread_source"] {
            Value::Null => false,
            Value::String(source) => source != "user",
            _ => true,
        };
        let mtime = fs::metadata(&path).ok()?.modified().ok()?;
        return Some(RolloutMeta {
            id,
            cwd,
            path,
            mtime,
            is_subagent,
        });
    }
    None
}

fn text_blocks(value: &Value) -> String {
    match value {
        Value::String(s) => s.clone(),
        Value::Array(items) => items
            .iter()
            .filter_map(|item| {
                item.get("text")
                    .and_then(Value::as_str)
                    .map(str::to_owned)
                    .or_else(|| {
                        item.get("output_text")
                            .and_then(Value::as_str)
                            .map(str::to_owned)
                    })
                    .or_else(|| item.as_str().map(str::to_owned))
            })
            .collect::<Vec<_>>()
            .join("\n"),
        Value::Null => String::new(),
        other => serde_json::to_string_pretty(other).unwrap_or_default(),
    }
}

fn extracted(
    target: Target,
    text: String,
    tool_name: Option<String>,
    timestamp: &str,
    session_id: &str,
    raw: Option<Value>,
) -> ExtractedContent {
    ExtractedContent {
        target,
        text,
        tool_name,
        timestamp: timestamp.to_string(),
        session_id: session_id.to_string(),
        edit_diff: None,
        raw_entry: raw,
    }
}

#[derive(Clone)]
struct CallInfo {
    name: String,
    shell: bool,
}

#[derive(Default)]
struct ExtractState {
    calls: HashMap<String, CallInfo>,
}

fn call_text(payload: &Value) -> String {
    payload["input"]
        .as_str()
        .or_else(|| payload["arguments"].as_str())
        .map(str::to_owned)
        .unwrap_or_else(|| text_blocks(&payload["input"]))
}

fn parse_args(text: &str) -> Option<Value> {
    serde_json::from_str(text).ok()
}

fn is_shell_tool(name: &str) -> bool {
    matches!(
        name.to_ascii_lowercase().as_str(),
        "exec" | "exec_command" | "shell" | "bash"
    )
}

fn normalize_custom_call(name: &str, text: &str) -> (String, String) {
    if name != "exec" {
        return (name.to_string(), text.to_string());
    }
    let nested = regex::Regex::new(r"tools\.([A-Za-z0-9_]+)\s*\(")
        .ok()
        .and_then(|re| re.captures(text))
        .and_then(|c| c.get(1))
        .map(|m| m.as_str().to_string());
    let Some(nested) = nested else {
        return (name.to_string(), text.to_string());
    };
    if nested == "apply_patch" {
        let patch = regex::Regex::new(r#"(?s)const\s+patch\s*=\s*(\"(?:\\.|[^\"\\])*\")\s*;"#)
            .ok()
            .and_then(|re| re.captures(text))
            .and_then(|c| c.get(1))
            .and_then(|m| serde_json::from_str::<String>(m.as_str()).ok())
            .unwrap_or_else(|| text.to_string());
        return (nested, patch);
    }
    (nested, text.to_string())
}

fn shell_command(name: &str, text: &str) -> Option<String> {
    if !is_shell_tool(name) {
        return None;
    }
    if let Some(v) = parse_args(text) {
        for key in ["cmd", "command", "script"] {
            if let Some(s) = v[key].as_str() {
                return Some(s.to_string());
            }
        }
    }
    // Current Codex custom `exec` records contain JavaScript wrapping an
    // exec_command call. Decode the JSON string literal used for its `cmd`.
    let re = regex::Regex::new(r#"[\"']cmd[\"']\s*:\s*(\"(?:\\.|[^\"\\])*\")"#).ok()?;
    let literal = re.captures(text)?.get(1)?.as_str();
    serde_json::from_str::<String>(literal).ok()
}

fn edit_diff(name: &str, text: &str) -> Option<EditDiff> {
    if !matches!(name.to_ascii_lowercase().as_str(), "edit" | "apply_patch") {
        return None;
    }
    let v = parse_args(text)?;
    let path = v["file_path"].as_str().or_else(|| v["filePath"].as_str())?;
    let old = v["old_string"]
        .as_str()
        .or_else(|| v["oldString"].as_str())?;
    let new = v["new_string"]
        .as_str()
        .or_else(|| v["newString"].as_str())?;
    Some(EditDiff {
        file_path: path.into(),
        old_string: old.into(),
        new_string: new.into(),
    })
}

fn normalized_raw(entry: &Value, target: &Target, session_id: &str) -> Value {
    let mut raw = entry.clone();
    if let Some(obj) = raw.as_object_mut() {
        obj.insert("sessionId".into(), Value::String(session_id.into()));
        obj.insert(
            "itemType".into(),
            obj.get("type").cloned().unwrap_or(Value::Null),
        );
        obj.insert("type".into(), Value::String(target.to_string()));
    }
    raw
}

fn extract_entry(
    entry: &Value,
    targets: &HashSet<Target>,
    session_id: &str,
    is_subagent: bool,
    keep_raw: bool,
    state: &mut ExtractState,
    out: &mut Vec<ExtractedContent>,
) {
    let payload = &entry["payload"];
    let timestamp = entry["timestamp"].as_str().unwrap_or("");
    let raw = |target: &Target| keep_raw.then(|| normalized_raw(entry, target, session_id));

    if entry["type"] == "event_msg" && payload["type"] == "user_message" {
        let target = if is_subagent {
            Target::SubagentPrompt
        } else {
            Target::User
        };
        if targets.contains(&target) {
            let text = payload["message"].as_str().unwrap_or("").to_string();
            if !text.is_empty() {
                out.push(extracted(
                    target.clone(),
                    text,
                    None,
                    timestamp,
                    session_id,
                    raw(&target),
                ));
            }
        }
        return;
    }
    if entry["type"] == "compacted"
        || (entry["type"] == "event_msg" && payload["type"] == "context_compacted")
    {
        if targets.contains(&Target::CompactSummary) {
            let text = payload["message"]
                .as_str()
                .filter(|s| !s.is_empty())
                .unwrap_or("(context compacted; Codex did not persist a plaintext summary)")
                .to_string();
            out.push(extracted(
                Target::CompactSummary,
                text,
                None,
                timestamp,
                session_id,
                raw(&Target::CompactSummary),
            ));
        }
        return;
    }
    if entry["type"] != "response_item" {
        return;
    }
    match payload["type"].as_str() {
        Some("message") => {
            let target = match payload["role"].as_str() {
                Some("assistant") if targets.contains(&Target::Assistant) => Target::Assistant,
                _ => return,
            };
            let text = text_blocks(&payload["content"]);
            if !text.is_empty() {
                let original = raw(&target);
                out.push(extracted(
                    target, text, None, timestamp, session_id, original,
                ));
            }
        }
        Some("reasoning") if targets.contains(&Target::Thinking) => {
            let text = text_blocks(&payload["summary"]);
            if !text.is_empty() {
                out.push(extracted(
                    Target::Thinking,
                    text,
                    None,
                    timestamp,
                    session_id,
                    raw(&Target::Thinking),
                ));
            }
        }
        Some("custom_tool_call") | Some("function_call") => {
            let native_name = payload["name"].as_str().unwrap_or("unknown");
            let native_text = call_text(payload);
            let (name, text) = normalize_custom_call(native_name, &native_text);
            let command = shell_command(&name, &text);
            let call_id = payload["call_id"]
                .as_str()
                .or_else(|| payload["id"].as_str());
            if let Some(id) = call_id {
                state.calls.insert(
                    id.into(),
                    CallInfo {
                        name: name.clone(),
                        shell: command.is_some(),
                    },
                );
            }
            if targets.contains(&Target::ToolUse) {
                let mut rec = extracted(
                    Target::ToolUse,
                    text.clone(),
                    Some(name.clone()),
                    timestamp,
                    session_id,
                    raw(&Target::ToolUse),
                );
                rec.edit_diff = edit_diff(&name, &text);
                out.push(rec);
            }
            if targets.contains(&Target::BashCommand) {
                if let Some(cmd) = command {
                    out.push(extracted(
                        Target::BashCommand,
                        cmd,
                        Some(name),
                        timestamp,
                        session_id,
                        raw(&Target::BashCommand),
                    ));
                }
            }
        }
        Some("custom_tool_call_output") | Some("function_call_output") => {
            let text = text_blocks(&payload["output"]);
            let info = payload["call_id"]
                .as_str()
                .and_then(|id| state.calls.get(id))
                .cloned();
            let name = info.as_ref().map(|i| i.name.clone());
            if targets.contains(&Target::ToolResult) {
                out.push(extracted(
                    Target::ToolResult,
                    text.clone(),
                    name.clone(),
                    timestamp,
                    session_id,
                    raw(&Target::ToolResult),
                ));
            }
            if targets.contains(&Target::BashOutput)
                && info.as_ref().map(|i| i.shell).unwrap_or(false)
            {
                out.push(extracted(
                    Target::BashOutput,
                    text,
                    name,
                    timestamp,
                    session_id,
                    raw(&Target::BashOutput),
                ));
            }
        }
        _ => {}
    }
}

impl Source for CodexSource {
    fn name(&self) -> &'static str {
        CODEX
    }

    fn discover_projects(&self) -> Vec<ProjectInfo> {
        let mut grouped: HashMap<String, (usize, std::time::SystemTime)> = HashMap::new();
        for r in self.rollouts() {
            let e = grouped
                .entry(r.cwd.clone())
                .or_insert((0, std::time::UNIX_EPOCH));
            e.0 += 1;
            e.1 = e.1.max(r.mtime);
        }
        let mut out: Vec<_> = grouped
            .into_iter()
            .map(|(cwd, (count, mtime))| ProjectInfo {
                encoded_path: cwd.clone(),
                decoded_path: cwd.clone(),
                verified: Path::new(&cwd).exists(),
                session_count: count,
                latest_mtime: Some(mtime),
                account: None,
                backend: CODEX,
            })
            .collect();
        out.sort_by_key(|p| std::cmp::Reverse(p.latest_mtime));
        out
    }

    fn discover_sessions(&self, project_path: &str) -> Vec<SessionFile> {
        let mut paths: HashSet<String> = get_worktree_paths(project_path).into_iter().collect();
        paths.insert(project_path.to_string());
        let mut out: Vec<_> = self
            .rollouts()
            .iter()
            .filter(|r| paths.contains(&r.cwd))
            .map(|r| SessionFile {
                session_id: r.id.clone(),
                file_path: r.path.clone(),
                mtime: r.mtime,
                is_subagent: r.is_subagent,
                backend: CODEX,
            })
            .collect();
        out.sort_by_key(|s| std::cmp::Reverse(s.mtime));
        out
    }

    fn extract_content(
        &self,
        session: &SessionFile,
        targets: &HashSet<Target>,
        keep_raw: bool,
    ) -> Vec<ExtractedContent> {
        let file = match fs::File::open(&session.file_path) {
            Ok(f) => f,
            Err(_) => return vec![],
        };
        let mut out = Vec::new();
        let mut state = ExtractState::default();
        for line in BufReader::new(file).lines().map_while(Result::ok) {
            if let Ok(entry) = serde_json::from_str(&line) {
                extract_entry(
                    &entry,
                    targets,
                    &session.session_id,
                    session.is_subagent,
                    keep_raw,
                    &mut state,
                    &mut out,
                );
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
        // Seed call correlation from the existing transcript so a result that
        // arrives just after follow begins still knows its tool name/type.
        let mut state = ExtractState::default();
        if let Ok(seed) = fs::File::open(&session.file_path) {
            let none = HashSet::new();
            let mut ignored = Vec::new();
            for line in BufReader::new(seed).lines().map_while(Result::ok) {
                if let Ok(entry) = serde_json::from_str(&line) {
                    extract_entry(
                        &entry,
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
        let mut file = fs::File::open(&session.file_path).map_err(|e| e.to_string())?;
        file.seek(SeekFrom::End(0)).map_err(|e| e.to_string())?;
        let mut reader = BufReader::new(file);
        loop {
            let mut line = String::new();
            match reader.read_line(&mut line) {
                Ok(0) => std::thread::sleep(std::time::Duration::from_millis(200)),
                Ok(_) => {
                    if let Ok(entry) = serde_json::from_str(&line) {
                        let mut out = Vec::new();
                        extract_entry(
                            &entry,
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
                    } else {
                        // Codex may be in the middle of appending a JSON value.
                        // Rewind the incomplete bytes and retry once the line is complete.
                        let bytes = line.len() as i64;
                        let inner = reader.get_mut();
                        let _ = inner.seek(SeekFrom::Current(-bytes));
                        reader = BufReader::new(reader.into_inner());
                        std::thread::sleep(std::time::Duration::from_millis(200));
                    }
                }
                Err(_) => std::thread::sleep(std::time::Duration::from_millis(200)),
            }
        }
    }

    fn discover_memory_files(&self, cwd: &Path, include_subdirs: bool) -> Vec<MemoryFile> {
        let mut files = discover_with_layout(
            cwd,
            &MemoryLayout {
                filenames: vec!["AGENTS.md".into()],
                managed_policy: None,
                config_dirs: vec![self.home.clone()],
                auto_memory: None,
            },
            include_subdirs,
        );
        files.extend(discover_native_memory_files(&self.home));
        files
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn discovers_and_extracts_rollout() {
        let tmp = tempfile::tempdir().unwrap();
        let sessions = tmp.path().join("sessions/2026/01/02");
        fs::create_dir_all(&sessions).unwrap();
        let path = sessions.join("rollout-test.jsonl");
        let mut f = fs::File::create(path).unwrap();
        writeln!(f, r#"{{"timestamp":"2026-01-02T00:00:00Z","type":"session_meta","payload":{{"id":"abc-123","cwd":"/work/repo","thread_source":"user"}}}}"#).unwrap();
        writeln!(f, r#"{{"timestamp":"2026-01-02T00:00:01Z","type":"event_msg","payload":{{"type":"user_message","message":"hello codex"}}}}"#).unwrap();
        writeln!(f, r#"{{"timestamp":"2026-01-02T00:00:02Z","type":"response_item","payload":{{"type":"custom_tool_call","call_id":"call-1","name":"exec_command","input":"{{\"cmd\":\"pwd\"}}"}}}}"#).unwrap();
        writeln!(f, r#"{{"timestamp":"2026-01-02T00:00:03Z","type":"response_item","payload":{{"type":"custom_tool_call_output","call_id":"call-1","output":"/work/repo"}}}}"#).unwrap();
        let source = CodexSource::new(tmp.path().to_path_buf());
        let projects = source.discover_projects();
        assert_eq!(projects[0].decoded_path, "/work/repo");
        let sessions = source.discover_sessions("/work/repo");
        assert_eq!(sessions[0].session_id, "abc-123");
        let targets = [
            Target::User,
            Target::ToolUse,
            Target::ToolResult,
            Target::BashCommand,
            Target::BashOutput,
        ]
        .into_iter()
        .collect();
        let records = source.extract_content(&sessions[0], &targets, false);
        assert_eq!(records.len(), 5);
        assert_eq!(records[0].text, "hello codex");
        assert_eq!(records[1].tool_name.as_deref(), Some("exec_command"));
        assert_eq!(records[2].target, Target::BashCommand);
        assert_eq!(records[2].text, "pwd");
        assert_eq!(records[3].tool_name.as_deref(), Some("exec_command"));
        assert_eq!(records[4].target, Target::BashOutput);

        let raw = source.extract_content(&sessions[0], &targets, true);
        assert_eq!(raw[0].raw_entry.as_ref().unwrap()["sessionId"], "abc-123");
        assert_eq!(raw[0].raw_entry.as_ref().unwrap()["type"], "user");
    }

    #[test]
    fn normalizes_wrapped_calls_and_edit_diffs() {
        let js = r#"const patch = \"*** Begin Patch\\n*** Update File: /x\\n-old\\n+new\\n*** End Patch\"; text(await tools.apply_patch(patch));"#;
        let (name, body) = normalize_custom_call("exec", js);
        assert_eq!(name, "apply_patch");
        assert!(body.contains("*** Update File: /x"));

        let edit = edit_diff(
            "edit",
            r#"{"file_path":"/x","old_string":"old","new_string":"new"}"#,
        )
        .unwrap();
        assert_eq!(edit.file_path, "/x");
        assert_eq!(edit.old_string, "old");
        assert_eq!(edit.new_string, "new");
    }

    #[test]
    fn filters_injected_user_context_and_maps_compaction_and_subagent_prompt() {
        let targets = [Target::User, Target::SubagentPrompt, Target::CompactSummary]
            .into_iter()
            .collect();
        let mut state = ExtractState::default();
        let mut out = Vec::new();
        let injected: Value = serde_json::from_str(r#"{"type":"response_item","timestamp":"t","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"<environment_context>secret</environment_context>"}]}}"#).unwrap();
        extract_entry(&injected, &targets, "s", false, false, &mut state, &mut out);
        assert!(out.is_empty());
        let user: Value = serde_json::from_str(r#"{"type":"event_msg","timestamp":"t","payload":{"type":"user_message","message":"delegated work"}}"#).unwrap();
        extract_entry(&user, &targets, "s", true, false, &mut state, &mut out);
        assert_eq!(out[0].target, Target::SubagentPrompt);
        let compact: Value = serde_json::from_str(
            r#"{"type":"event_msg","timestamp":"t","payload":{"type":"context_compacted"}}"#,
        )
        .unwrap();
        extract_entry(&compact, &targets, "s", false, false, &mut state, &mut out);
        assert_eq!(out[1].target, Target::CompactSummary);
    }

    #[test]
    fn discovers_native_memory_tree_in_codex_load_order() {
        let tmp = tempfile::tempdir().unwrap();
        let project = tmp.path().join("project");
        let memories = tmp.path().join("memories");
        fs::create_dir_all(project.join("nested")).unwrap();
        fs::create_dir_all(memories.join("rollout_summaries")).unwrap();
        fs::create_dir_all(memories.join("skills/example")).unwrap();
        fs::create_dir_all(memories.join("extensions/import/resources")).unwrap();
        fs::create_dir_all(memories.join(".git")).unwrap();

        fs::write(tmp.path().join("AGENTS.md"), "global instructions").unwrap();
        fs::write(project.join("nested/AGENTS.md"), "nested instructions").unwrap();
        fs::write(memories.join("memory_summary.md"), "summary").unwrap();
        fs::write(memories.join("MEMORY.md"), "registry").unwrap();
        fs::write(memories.join("raw_memories.md"), "raw details").unwrap();
        fs::write(memories.join("rollout_summaries/session.md"), "rollout").unwrap();
        fs::write(memories.join("skills/example/SKILL.md"), "skill").unwrap();
        fs::write(
            memories.join("extensions/import/resources/topic.md"),
            "imported topic",
        )
        .unwrap();
        fs::write(
            memories.join("phase2_workspace_diff.md"),
            "temporary prompt input",
        )
        .unwrap();
        fs::write(memories.join(".git/hidden.md"), "git internals").unwrap();
        fs::write(memories.join("ignored.txt"), "not markdown").unwrap();

        let source = CodexSource::new(tmp.path().to_path_buf());
        let files = source.discover_memory_files(&project, false);
        let native: Vec<_> = files
            .iter()
            .filter(|file| file.path.starts_with(&memories))
            .map(|file| {
                (
                    file.path.strip_prefix(&memories).unwrap().to_path_buf(),
                    file.source,
                )
            })
            .collect();

        assert_eq!(files[0].path, tmp.path().join("AGENTS.md"));
        assert_eq!(
            native,
            vec![
                (
                    PathBuf::from("memory_summary.md"),
                    MemorySource::CodexMemorySummary
                ),
                (
                    PathBuf::from("MEMORY.md"),
                    MemorySource::CodexMemoryRegistry
                ),
                (
                    PathBuf::from("extensions/import/resources/topic.md"),
                    MemorySource::CodexMemoryFile,
                ),
                (
                    PathBuf::from("raw_memories.md"),
                    MemorySource::CodexMemoryFile,
                ),
                (
                    PathBuf::from("rollout_summaries/session.md"),
                    MemorySource::CodexMemoryFile,
                ),
                (
                    PathBuf::from("skills/example/SKILL.md"),
                    MemorySource::CodexMemoryFile,
                ),
            ]
        );
        assert!(!files
            .iter()
            .any(|file| file.path.ends_with("nested/AGENTS.md")));
        assert!(!files
            .iter()
            .any(|file| file.path.ends_with("phase2_workspace_diff.md")));
        assert!(!files
            .iter()
            .any(|file| file.path.ends_with(".git/hidden.md")));
    }
}
