use std::fs;
use std::io::{BufRead, BufReader};
use std::path::Path;
use std::collections::HashMap;

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum Target {
    User,
    Assistant,
    BashCommand,
    BashOutput,
    ToolUse,
    ToolResult,
    SubagentPrompt,
    CompactSummary,
    System,
    FileHistorySnapshot,
    QueueOperation,
    LastPrompt,
    AgentName,
    CustomTitle,
}

impl Target {
    pub fn as_str(&self) -> &'static str {
        match self {
            Target::User => "user",
            Target::Assistant => "assistant",
            Target::BashCommand => "bash-command",
            Target::BashOutput => "bash-output",
            Target::ToolUse => "tool-use",
            Target::ToolResult => "tool-result",
            Target::SubagentPrompt => "subagent-prompt",
            Target::CompactSummary => "compact-summary",
            Target::System => "system",
            Target::FileHistorySnapshot => "file-history-snapshot",
            Target::QueueOperation => "queue-operation",
            Target::LastPrompt => "last-prompt",
            Target::AgentName => "agent-name",
            Target::CustomTitle => "custom-title",
        }
    }
}

/// Diff data extracted from an Edit tool call.
pub struct EditDiff {
    pub file_path: String,
    pub old_string: String,
    pub new_string: String,
}

pub struct ExtractedContent {
    pub target: Target,
    pub text: String,
    pub tool_name: Option<String>,
    pub timestamp: String,
    pub session_id: String,
    /// Populated for Edit tool calls; `None` for everything else.
    pub edit_diff: Option<EditDiff>,
    /// The original JSONL entry, preserved when `keep_raw` is set.
    pub raw_entry: Option<serde_json::Value>,
}

pub type ToolUseMap = HashMap<String, String>;

pub fn collect_tool_use_ids(entry: &serde_json::Value, map: &mut ToolUseMap) {
    let content = match entry["type"].as_str() {
        Some("assistant") => &entry["message"]["content"],
        Some("progress") => &entry["data"]["message"]["message"]["content"],
        _ => return,
    };

    if let Some(arr) = content.as_array() {
        for block in arr {
            if block["type"] == "tool_use" {
                if let (Some(id), Some(name)) = (block["id"].as_str(), block["name"].as_str()) {
                    map.insert(id.to_string(), name.to_string());
                }
            }
        }
    }
}

#[allow(dead_code)]
pub fn extract_content(
    path: &Path,
    targets: &std::collections::HashSet<Target>,
    session_id: &str,
    is_subagent: bool,
) -> Vec<ExtractedContent> {
    extract_content_opts(path, targets, session_id, is_subagent, false)
}

pub fn extract_content_opts(
    path: &Path,
    targets: &std::collections::HashSet<Target>,
    session_id: &str,
    is_subagent: bool,
    keep_raw: bool,
) -> Vec<ExtractedContent> {
    let file = match fs::File::open(path) {
        Ok(f) => f,
        Err(e) => {
            eprintln!("warning: failed to open {}: {}", path.display(), e);
            return vec![];
        }
    };

    let mut tool_use_map = ToolUseMap::new();
    let mut results = vec![];
    for (line_num, line_result) in BufReader::new(file).lines().enumerate() {
        let line = match line_result {
            Ok(l) => l,
            Err(_) => continue,
        };
        if line.is_empty() {
            continue;
        }
        match serde_json::from_str::<serde_json::Value>(&line) {
            Ok(entry) => {
                collect_tool_use_ids(&entry, &mut tool_use_map);
                let before = results.len();
                extract_from_entry(&entry, &tool_use_map, targets, session_id, is_subagent, &mut results);
                if keep_raw {
                    for r in &mut results[before..] {
                        r.raw_entry = Some(entry.clone());
                    }
                }
            }
            Err(e) => eprintln!("warning: {}: line {}: {}", path.display(), line_num + 1, e),
        }
    }
    results
}

pub fn extract_from_entry(
    entry: &serde_json::Value,
    tool_use_map: &ToolUseMap,
    targets: &std::collections::HashSet<Target>,
    session_id: &str,
    is_subagent: bool,
    out: &mut Vec<ExtractedContent>,
) {
    let timestamp = entry["timestamp"].as_str().unwrap_or("").to_string();
    let entry_session = entry["sessionId"].as_str().unwrap_or(session_id);

    match entry["type"].as_str() {
        Some("user") => extract_user(entry, tool_use_map, targets, entry_session, &timestamp, is_subagent, out),
        Some("assistant") => extract_assistant(entry, targets, entry_session, &timestamp, out),
        Some("progress") => {
            // Synthesize nested entry
            let nested_type = entry["data"]["message"]["type"].as_str();
            if let Some(inner_msg) = entry["data"]["message"]["message"].as_object() {
                let synth_type = if nested_type == Some("assistant") { "assistant" } else { "user" };
                let synth = serde_json::json!({
                    "type": synth_type,
                    "message": inner_msg,
                    "timestamp": timestamp,
                    "sessionId": entry_session,
                });
                extract_from_entry(&synth, tool_use_map, targets, session_id, is_subagent, out);
            }
        }
        Some("file-history-snapshot") => {
            if targets.contains(&Target::FileHistorySnapshot) {
                let snap_ts = entry["snapshot"]["timestamp"].as_str().unwrap_or("");
                let backups = entry["snapshot"]["trackedFileBackups"].as_object();
                let lines: Vec<String> = backups
                    .map(|m| m.iter().map(|(k, v)| {
                        let version = v["version"].as_u64().unwrap_or(0);
                        format!("{} (v{})", k, version)
                    }).collect())
                    .unwrap_or_default();
                let text = if lines.is_empty() {
                    "(no tracked files)".to_string()
                } else {
                    lines.join("\n")
                };
                out.push(ExtractedContent {
                    target: Target::FileHistorySnapshot,
                    text,
                    tool_name: None,
                    timestamp: snap_ts.to_string(),
                    session_id: entry_session.to_string(),
                    edit_diff: None,
                    raw_entry: None,
                });
            }
        }
        Some("system") => {
            if targets.contains(&Target::System) {
                let subtype = entry["subtype"].as_str().unwrap_or("unknown");
                let text = entry["content"].as_str().unwrap_or(subtype);
                out.push(ExtractedContent {
                    target: Target::System,
                    text: text.to_string(),
                    tool_name: Some(subtype.to_string()),
                    timestamp: timestamp.to_string(),
                    session_id: entry_session.to_string(),
                    edit_diff: None,
                    raw_entry: None,
                });
            }
        }
        Some("queue-operation") => {
            if targets.contains(&Target::QueueOperation) {
                let operation = entry["operation"].as_str().unwrap_or("unknown");
                let text = entry["content"].as_str().unwrap_or(operation);
                out.push(ExtractedContent {
                    target: Target::QueueOperation,
                    text: text.to_string(),
                    tool_name: Some(operation.to_string()),
                    timestamp: timestamp.to_string(),
                    session_id: entry_session.to_string(),
                    edit_diff: None,
                    raw_entry: None,
                });
            }
        }
        Some("last-prompt") => {
            if targets.contains(&Target::LastPrompt) {
                let text = entry["lastPrompt"].as_str().unwrap_or("");
                out.push(ExtractedContent {
                    target: Target::LastPrompt,
                    text: text.to_string(),
                    tool_name: None,
                    timestamp: timestamp.to_string(),
                    session_id: entry_session.to_string(),
                    edit_diff: None,
                    raw_entry: None,
                });
            }
        }
        Some("agent-name") => {
            if targets.contains(&Target::AgentName) {
                let text = entry["agentName"].as_str().unwrap_or("");
                out.push(ExtractedContent {
                    target: Target::AgentName,
                    text: text.to_string(),
                    tool_name: None,
                    timestamp: timestamp.to_string(),
                    session_id: entry_session.to_string(),
                    edit_diff: None,
                    raw_entry: None,
                });
            }
        }
        Some("custom-title") => {
            if targets.contains(&Target::CustomTitle) {
                let text = entry["customTitle"].as_str().unwrap_or("");
                out.push(ExtractedContent {
                    target: Target::CustomTitle,
                    text: text.to_string(),
                    tool_name: None,
                    timestamp: timestamp.to_string(),
                    session_id: entry_session.to_string(),
                    edit_diff: None,
                    raw_entry: None,
                });
            }
        }
        _ => {
            let raw = entry.to_string();
            let preview: String = raw.chars().take(120).collect();
            let ellipsis = if raw.chars().count() > 120 { "..." } else { "" };
            eprintln!("warning: skipping unrecognized record: {}{}", preview, ellipsis);
        }
    }
}

fn extract_user(
    entry: &serde_json::Value,
    tool_use_map: &ToolUseMap,
    targets: &std::collections::HashSet<Target>,
    session_id: &str,
    timestamp: &str,
    is_subagent: bool,
    out: &mut Vec<ExtractedContent>,
) {
    let content = &entry["message"]["content"];

    // Determine user target type
    let user_target = if entry["isCompactSummary"] == true {
        if targets.contains(&Target::CompactSummary) { Some(Target::CompactSummary) } else { None }
    } else if is_subagent {
        if targets.contains(&Target::SubagentPrompt) { Some(Target::SubagentPrompt) } else { None }
    } else {
        if targets.contains(&Target::User) { Some(Target::User) } else { None }
    };

    if let Some(target) = user_target {
        if let Some(text) = content.as_str() {
            out.push(ExtractedContent {
                target,
                text: text.to_string(),
                tool_name: None,
                timestamp: timestamp.to_string(),
                session_id: session_id.to_string(),
                edit_diff: None,
                raw_entry: None,
            });
        } else if let Some(arr) = content.as_array() {
            for block in arr {
                if block["type"] == "text" {
                    if let Some(text) = block["text"].as_str() {
                        out.push(ExtractedContent {
                            target: target.clone(),
                            text: text.to_string(),
                            tool_name: None,
                            timestamp: timestamp.to_string(),
                            session_id: session_id.to_string(),
                            edit_diff: None,
                            raw_entry: None,
                        });
                    }
                }
            }
        }
    }

    // Tool results
    if let Some(arr) = content.as_array() {
        for block in arr {
            if block["type"] != "tool_result" {
                continue;
            }
            let tool_use_id = match block["tool_use_id"].as_str() {
                Some(id) => id,
                None => continue,
            };
            let tool_name = tool_use_map.get(tool_use_id).cloned().unwrap_or_default();
            let is_bash = tool_name == "Bash";
            let target = if is_bash { Target::BashOutput } else { Target::ToolResult };

            if !targets.contains(&target) {
                continue;
            }

            if let Some(text) = extract_tool_result_text(block) {
                out.push(ExtractedContent {
                    target,
                    text,
                    tool_name: Some(tool_name),
                    timestamp: timestamp.to_string(),
                    session_id: session_id.to_string(),
                    edit_diff: None,
                    raw_entry: None,
                });
            }
        }
    }
}

fn extract_tool_result_text(block: &serde_json::Value) -> Option<String> {
    if let Some(text) = block["content"].as_str() {
        return Some(text.to_string());
    }
    if let Some(arr) = block["content"].as_array() {
        let parts: Vec<_> = arr.iter()
            .filter_map(|item| item["text"].as_str())
            .collect();
        if !parts.is_empty() {
            return Some(parts.join("\n"));
        }
    }
    None
}

fn format_tool_input(input: &serde_json::Value) -> String {
    let obj = match input.as_object() {
        Some(o) => o,
        None => return String::new(),
    };
    let mut lines = vec![];
    for (key, value) in obj {
        if let Some(s) = value.as_str() {
            if s.contains('\n') {
                lines.push(format!("{}:\n{}", key, s));
            } else {
                lines.push(format!("{}: {}", key, s));
            }
        } else if value.is_number() || value.is_boolean() {
            lines.push(format!("{}: {}", key, value));
        } else if !value.is_null() {
            lines.push(format!("{}: {}", key, value));
        }
    }
    lines.join("\n")
}

fn extract_assistant(
    entry: &serde_json::Value,
    targets: &std::collections::HashSet<Target>,
    session_id: &str,
    timestamp: &str,
    out: &mut Vec<ExtractedContent>,
) {
    let content = match entry["message"]["content"].as_array() {
        Some(a) => a,
        None => return,
    };

    for block in content {
        if block["type"] == "text" && targets.contains(&Target::Assistant) {
            if let Some(text) = block["text"].as_str() {
                out.push(ExtractedContent {
                    target: Target::Assistant,
                    text: text.to_string(),
                    tool_name: None,
                    timestamp: timestamp.to_string(),
                    session_id: session_id.to_string(),
                    edit_diff: None,
                    raw_entry: None,
                });
            }
        }

        if block["type"] == "tool_use" {
            let name = block["name"].as_str().unwrap_or("").to_string();
            let input = &block["input"];

            if name == "Bash" && targets.contains(&Target::BashCommand) {
                if let Some(cmd) = input["command"].as_str() {
                    out.push(ExtractedContent {
                        target: Target::BashCommand,
                        text: cmd.to_string(),
                        tool_name: Some("Bash".to_string()),
                        timestamp: timestamp.to_string(),
                        session_id: session_id.to_string(),
                        edit_diff: None,
                        raw_entry: None,
                    });
                }
            }

            if targets.contains(&Target::ToolUse) && !name.is_empty() {
                let edit_diff = if name == "Edit" {
                    match (
                        input["file_path"].as_str(),
                        input["old_string"].as_str(),
                        input["new_string"].as_str(),
                    ) {
                        (Some(fp), Some(old), Some(new)) => Some(EditDiff {
                            file_path: fp.to_string(),
                            old_string: old.to_string(),
                            new_string: new.to_string(),
                        }),
                        _ => None,
                    }
                } else {
                    None
                };
                out.push(ExtractedContent {
                    target: Target::ToolUse,
                    text: format_tool_input(input),
                    tool_name: Some(name),
                    timestamp: timestamp.to_string(),
                    session_id: session_id.to_string(),
                    edit_diff,
                    raw_entry: None,
                });
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;
    use std::io::Write;

    fn write_jsonl(lines: &[&str]) -> tempfile::NamedTempFile {
        let mut f = tempfile::NamedTempFile::new().unwrap();
        for line in lines {
            writeln!(f, "{}", line).unwrap();
        }
        f.flush().unwrap();
        f
    }

    fn all_targets() -> HashSet<Target> {
        [
            Target::User, Target::Assistant, Target::BashCommand, Target::BashOutput,
            Target::ToolUse, Target::ToolResult, Target::SubagentPrompt, Target::CompactSummary,
            Target::System, Target::FileHistorySnapshot, Target::QueueOperation,
            Target::LastPrompt, Target::AgentName, Target::CustomTitle,
        ].into_iter().collect()
    }

    #[test]
    fn test_extract_user_text_message() {
        let f = write_jsonl(&[
            r#"{"type":"user","message":{"role":"user","content":"hello world"},"timestamp":"2024-01-01T00:00:00Z","sessionId":"test-session"}"#,
        ]);
        let contents = extract_content(f.path(), &all_targets(), "test-session", false);
        assert_eq!(contents.len(), 1);
        assert_eq!(contents[0].text, "hello world");
        assert_eq!(contents[0].target, Target::User);
    }

    #[test]
    fn test_extract_assistant_text() {
        let f = write_jsonl(&[
            r#"{"type":"assistant","message":{"role":"assistant","content":[{"type":"text","text":"hi there"}]},"timestamp":"2024-01-01T00:00:00Z","sessionId":"s"}"#,
        ]);
        let contents = extract_content(f.path(), &all_targets(), "s", false);
        assert_eq!(contents.len(), 1);
        assert_eq!(contents[0].text, "hi there");
        assert_eq!(contents[0].target, Target::Assistant);
    }

    #[test]
    fn test_extract_bash_command() {
        let f = write_jsonl(&[
            r#"{"type":"assistant","message":{"content":[{"type":"tool_use","id":"tu1","name":"Bash","input":{"command":"ls -la"}}]},"timestamp":"2024-01-01T00:00:00Z","sessionId":"s"}"#,
        ]);
        let contents = extract_content(f.path(), &all_targets(), "s", false);
        let bash = contents.iter().find(|c| c.target == Target::BashCommand);
        assert!(bash.is_some());
        assert_eq!(bash.unwrap().text, "ls -la");
    }

    #[test]
    fn test_extract_bash_output() {
        // First write a tool_use entry so we can map the ID
        let f = write_jsonl(&[
            r#"{"type":"assistant","message":{"content":[{"type":"tool_use","id":"tu1","name":"Bash","input":{"command":"ls"}}]},"timestamp":"2024-01-01T00:00:00Z","sessionId":"s"}"#,
            r#"{"type":"user","message":{"content":[{"type":"tool_result","tool_use_id":"tu1","content":"file1.txt\nfile2.txt"}]},"timestamp":"2024-01-01T00:00:01Z","sessionId":"s"}"#,
        ]);
        let contents = extract_content(f.path(), &all_targets(), "s", false);
        let output = contents.iter().find(|c| c.target == Target::BashOutput);
        assert!(output.is_some());
        assert!(output.unwrap().text.contains("file1.txt"));
    }

    #[test]
    fn test_extract_compact_summary() {
        let f = write_jsonl(&[
            r#"{"type":"user","isCompactSummary":true,"message":{"role":"user","content":"Summary of prior conversation"},"timestamp":"2024-01-01T00:00:00Z","sessionId":"s"}"#,
        ]);
        let contents = extract_content(f.path(), &all_targets(), "s", false);
        assert_eq!(contents.len(), 1);
        assert_eq!(contents[0].target, Target::CompactSummary);
    }

    #[test]
    fn test_extract_subagent_prompt() {
        let f = write_jsonl(&[
            r#"{"type":"user","message":{"role":"user","content":"Do the thing"},"timestamp":"2024-01-01T00:00:00Z","sessionId":"s"}"#,
        ]);
        let contents = extract_content(f.path(), &all_targets(), "s", true); // is_subagent=true
        assert_eq!(contents.len(), 1);
        assert_eq!(contents[0].target, Target::SubagentPrompt);
    }

    #[test]
    fn test_warns_on_unrecognized_record_type() {
        let f = write_jsonl(&[
            r#"{"type":"totally_unknown","content":"something","timestamp":"2024-01-01T00:00:00Z","sessionId":"s"}"#,
            r#"{"type":"user","message":{"content":"hello"},"timestamp":"2024-01-01T00:00:01Z","sessionId":"s"}"#,
        ]);
        // Capture stderr by reading after the fact — we just check it doesn't panic
        // and that the user message is still extracted.
        let contents = extract_content(f.path(), &all_targets(), "s", false);
        assert_eq!(contents.len(), 1);
        assert_eq!(contents[0].target, Target::User);
        // (The warning goes to stderr; tested at e2e level for the actual message text)
    }

    #[test]
    fn test_skips_malformed_lines() {
        let f = write_jsonl(&[
            "this is not json at all!!",
            r#"{"type":"user","message":{"content":"valid"},"timestamp":"2024-01-01T00:00:00Z","sessionId":"s"}"#,
        ]);
        let contents = extract_content(f.path(), &all_targets(), "s", false);
        // Should parse the valid line and skip the malformed one
        assert_eq!(contents.len(), 1);
    }

    #[test]
    fn test_target_filtering() {
        let f = write_jsonl(&[
            r#"{"type":"user","message":{"content":"user message"},"timestamp":"2024-01-01T00:00:00Z","sessionId":"s"}"#,
            r#"{"type":"assistant","message":{"content":[{"type":"text","text":"assistant reply"}]},"timestamp":"2024-01-01T00:00:00Z","sessionId":"s"}"#,
        ]);
        // Only user target
        let user_only: HashSet<Target> = [Target::User].into_iter().collect();
        let contents = extract_content(f.path(), &user_only, "s", false);
        assert_eq!(contents.len(), 1);
        assert_eq!(contents[0].target, Target::User);
    }

    #[test]
    fn test_extract_edit_tool_populates_edit_diff() {
        let f = write_jsonl(&[
            r#"{"type":"assistant","message":{"content":[{"type":"tool_use","id":"tu1","name":"Edit","input":{"file_path":"/src/lib.rs","old_string":"fn old() {}","new_string":"fn new() {}"}}]},"timestamp":"2024-01-01T00:00:00Z","sessionId":"s"}"#,
        ]);
        let contents = extract_content(f.path(), &all_targets(), "s", false);
        let edit = contents.iter().find(|c| c.target == Target::ToolUse && c.tool_name.as_deref() == Some("Edit"));
        assert!(edit.is_some(), "should extract Edit tool-use");
        let diff = edit.unwrap().edit_diff.as_ref().expect("edit_diff should be populated");
        assert_eq!(diff.file_path, "/src/lib.rs");
        assert_eq!(diff.old_string, "fn old() {}");
        assert_eq!(diff.new_string, "fn new() {}");
    }

    #[test]
    fn test_non_edit_tool_has_no_edit_diff() {
        let f = write_jsonl(&[
            r#"{"type":"assistant","message":{"content":[{"type":"tool_use","id":"tu1","name":"Read","input":{"file_path":"/foo.rs"}}]},"timestamp":"2024-01-01T00:00:00Z","sessionId":"s"}"#,
        ]);
        let contents = extract_content(f.path(), &all_targets(), "s", false);
        let read = contents.iter().find(|c| c.target == Target::ToolUse && c.tool_name.as_deref() == Some("Read"));
        assert!(read.is_some());
        assert!(read.unwrap().edit_diff.is_none(), "non-Edit tools should not have edit_diff");
    }

    #[test]
    fn test_edit_tool_missing_fields_has_no_edit_diff() {
        // Edit tool call missing new_string — should not panic, edit_diff should be None
        let f = write_jsonl(&[
            r#"{"type":"assistant","message":{"content":[{"type":"tool_use","id":"tu1","name":"Edit","input":{"file_path":"/x.rs","old_string":"foo"}}]},"timestamp":"2024-01-01T00:00:00Z","sessionId":"s"}"#,
        ]);
        let contents = extract_content(f.path(), &all_targets(), "s", false);
        let edit = contents.iter().find(|c| c.tool_name.as_deref() == Some("Edit"));
        assert!(edit.is_some());
        assert!(edit.unwrap().edit_diff.is_none(), "incomplete Edit input should have no edit_diff");
    }

    #[test]
    fn test_extract_agent_name() {
        let f = write_jsonl(&[
            r#"{"type":"agent-name","agentName":"my-agent","sessionId":"s"}"#,
        ]);
        let contents = extract_content(f.path(), &all_targets(), "s", false);
        let an = contents.iter().find(|c| c.target == Target::AgentName);
        assert!(an.is_some(), "should extract agent-name record");
        assert_eq!(an.unwrap().text, "my-agent");
    }

    #[test]
    fn test_extract_custom_title() {
        let f = write_jsonl(&[
            r#"{"type":"custom-title","customTitle":"my-title","sessionId":"s"}"#,
        ]);
        let contents = extract_content(f.path(), &all_targets(), "s", false);
        let ct = contents.iter().find(|c| c.target == Target::CustomTitle);
        assert!(ct.is_some(), "should extract custom-title record");
        assert_eq!(ct.unwrap().text, "my-title");
    }

    #[test]
    fn test_agent_name_not_warned_as_unrecognized() {
        let f = write_jsonl(&[
            r#"{"type":"agent-name","agentName":"test","sessionId":"s"}"#,
        ]);
        // Just ensure it doesn't panic and produces a result
        let targets: HashSet<Target> = [Target::AgentName].into_iter().collect();
        let contents = extract_content(f.path(), &targets, "s", false);
        assert_eq!(contents.len(), 1);
    }

    #[test]
    fn test_custom_title_not_warned_as_unrecognized() {
        let f = write_jsonl(&[
            r#"{"type":"custom-title","customTitle":"test","sessionId":"s"}"#,
        ]);
        let targets: HashSet<Target> = [Target::CustomTitle].into_iter().collect();
        let contents = extract_content(f.path(), &targets, "s", false);
        assert_eq!(contents.len(), 1);
    }
}
