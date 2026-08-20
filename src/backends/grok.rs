//! Grok Build backend: reads durable ACP updates from
//! `$GROK_HOME/sessions/<encoded-cwd>/<session-id>/updates.jsonl`.

use std::collections::{HashMap, HashSet};
use std::fs;
use std::io::{BufRead, BufReader, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::OnceLock;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde_json::{json, Value};

use crate::memory::{discover_with_layout, MemoryFile, MemoryLayout, MemorySource};
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

fn collect_nested_grok_rules(dir: &Path, out: &mut Vec<PathBuf>) {
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
        if matches!(
            name.as_ref(),
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
        ) {
            continue;
        }
        if name == ".grok" {
            out.extend(direct_markdown(&path.join("rules")));
        } else {
            collect_nested_grok_rules(&path, out);
        }
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
        Value::Object(object) => object
            .get("text")
            .and_then(Value::as_str)
            .map(str::to_owned)
            .or_else(|| {
                object
                    .get("content")
                    .map(content_text)
                    .filter(|text| !text.is_empty())
            })
            .unwrap_or_default(),
        _ => String::new(),
    }
}

fn format_tool_input(input: &Value) -> String {
    let Some(object) = input.as_object() else {
        return String::new();
    };
    object
        .iter()
        .filter(|(key, value)| *key != "variant" && !value.is_null())
        .map(|(key, value)| match value.as_str() {
            Some(text) if text.contains('\n') => format!("{}:\n{}", key, text),
            Some(text) => format!("{}: {}", key, text),
            None => format!("{}: {}", key, value),
        })
        .collect::<Vec<_>>()
        .join("\n")
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
    for path in [
        &update["rawOutput"]["output_for_prompt"],
        &update["rawOutput"]["content"],
        &update["rawOutput"]["Content"]["content"],
        &update["rawOutput"]["FileNotFound"],
        &update["rawOutput"]["error"],
    ] {
        if let Some(text) = path.as_str().filter(|text| !text.is_empty()) {
            return text.to_string();
        }
    }
    String::new()
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
        "session_recap" if targets.contains(&Target::CompactSummary) => {
            if let Some(text) = update["summary"].as_str().filter(|text| !text.is_empty()) {
                out.push(extracted(
                    Target::CompactSummary,
                    text.to_string(),
                    None,
                    &timestamp,
                    session_id,
                    raw(&Target::CompactSummary),
                ));
            }
        }
        _ => {}
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

fn append_title(
    path: &Path,
    targets: &HashSet<Target>,
    session_id: &str,
    keep_raw: bool,
    out: &mut Vec<ExtractedContent>,
) {
    let Some((title, manual, timestamp)) = summary_title(path) else {
        return;
    };
    let target = if manual {
        Target::CustomTitle
    } else {
        Target::AiTitle
    };
    if !targets.contains(&target) {
        return;
    }
    let raw = keep_raw.then(|| {
        json!({
            "sessionId": session_id,
            "timestamp": timestamp,
            "type": target.to_string(),
            "itemType": "summary-title",
            "title": title,
        })
    });
    out.push(extracted(target, title, None, &timestamp, session_id, raw));
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
        if let Some(summary_path) = session
            .file_path
            .parent()
            .map(|dir| dir.join("summary.json"))
        {
            append_title(
                &summary_path,
                targets,
                &session.session_id,
                keep_raw,
                &mut out,
            );
        }
        out
    }

    fn follow(
        &self,
        session: &SessionFile,
        targets: &HashSet<Target>,
        on_records: &mut dyn FnMut(&[ExtractedContent]),
    ) -> Result<(), String> {
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
        let mut file = fs::File::open(&session.file_path).map_err(|error| error.to_string())?;
        file.seek(SeekFrom::End(0))
            .map_err(|error| error.to_string())?;
        let mut reader = BufReader::new(file);
        loop {
            let mut line = String::new();
            match reader.read_line(&mut line) {
                Ok(0) => std::thread::sleep(Duration::from_millis(200)),
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
                        let bytes = line.len() as i64;
                        let inner = reader.get_mut();
                        let _ = inner.seek(SeekFrom::Current(-bytes));
                        reader = BufReader::new(reader.into_inner());
                        std::thread::sleep(Duration::from_millis(200));
                    }
                }
                Err(_) => std::thread::sleep(Duration::from_millis(200)),
            }
        }
    }

    fn discover_memory_files(&self, cwd: &Path, include_subdirs: bool) -> Vec<MemoryFile> {
        let layout = MemoryLayout {
            filenames: grok_instruction_names()
                .into_iter()
                .map(str::to_string)
                .collect(),
            managed_policy: None,
            config_dirs: vec![self.home.clone()],
            auto_memory: None,
        };
        let mut out = discover_with_layout(cwd, &layout, include_subdirs);
        let root = git_root(cwd);
        out.retain(|file| {
            !matches!(file.source, MemorySource::Ancestor | MemorySource::Subdir)
                || file.path.starts_with(&root)
        });
        out.extend(
            direct_markdown(&self.home.join("rules"))
                .into_iter()
                .map(|path| MemoryFile {
                    path,
                    source: MemorySource::UserGlobal,
                    imported_by: None,
                }),
        );
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
        for dir in chain {
            out.extend(
                direct_markdown(&dir.join(".grok/rules"))
                    .into_iter()
                    .map(|path| MemoryFile {
                        path,
                        source: MemorySource::Ancestor,
                        imported_by: None,
                    }),
            );
        }
        if include_subdirs {
            let mut paths = Vec::new();
            collect_nested_grok_rules(cwd, &mut paths);
            paths.sort();
            out.extend(paths.into_iter().map(|path| MemoryFile {
                path,
                source: MemorySource::Subdir,
                imported_by: None,
            }));
        }
        let memory_root = self.home.join("memory");
        let global = memory_root.join("MEMORY.md");
        if global.is_file() {
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
        out.extend(paths.into_iter().map(|path| MemoryFile {
            path,
            source: MemorySource::GrokMemoryWorkspace,
            imported_by: None,
        }));
        let mut seen = HashSet::new();
        out.retain(|file| seen.insert(file.path.clone()));
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
