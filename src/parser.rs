use std::fs;
use std::io::{BufRead, BufReader};
use std::path::Path;
use std::collections::HashMap;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Target {
    User,
    Assistant,
    BashCommand,
    BashOutput,
    ToolUse,
    ToolResult,
    SubagentPrompt,
    CompactSummary,
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
        }
    }
}

pub struct ExtractedContent {
    pub target: Target,
    pub text: String,
    pub tool_name: Option<String>,
    pub timestamp: String,
    pub session_id: String,
}

type ToolUseMap = HashMap<String, String>;

fn collect_tool_use_ids(entry: &serde_json::Value, map: &mut ToolUseMap) {
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

pub fn extract_content(
    path: &Path,
    targets: &std::collections::HashSet<String>,
    session_id: &str,
    is_subagent: bool,
) -> Vec<ExtractedContent> {
    let file = match fs::File::open(path) {
        Ok(f) => f,
        Err(_) => return vec![],
    };

    let mut tool_use_map = ToolUseMap::new();
    let mut results = vec![];
    for line in BufReader::new(file).lines().flatten() {
        if line.is_empty() {
            continue;
        }
        if let Ok(entry) = serde_json::from_str::<serde_json::Value>(&line) {
            collect_tool_use_ids(&entry, &mut tool_use_map);
            extract_from_entry(&entry, &tool_use_map, targets, session_id, is_subagent, &mut results);
        }
    }
    results
}

fn extract_from_entry(
    entry: &serde_json::Value,
    tool_use_map: &ToolUseMap,
    targets: &std::collections::HashSet<String>,
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
        _ => {}
    }
}

fn extract_user(
    entry: &serde_json::Value,
    tool_use_map: &ToolUseMap,
    targets: &std::collections::HashSet<String>,
    session_id: &str,
    timestamp: &str,
    is_subagent: bool,
    out: &mut Vec<ExtractedContent>,
) {
    let content = &entry["message"]["content"];

    // Determine user target type
    let user_target = if entry["isCompactSummary"] == true {
        if targets.contains("compact-summary") { Some(Target::CompactSummary) } else { None }
    } else if is_subagent {
        if targets.contains("subagent-prompt") { Some(Target::SubagentPrompt) } else { None }
    } else {
        if targets.contains("user") { Some(Target::User) } else { None }
    };

    if let Some(target) = user_target {
        if let Some(text) = content.as_str() {
            out.push(ExtractedContent {
                target,
                text: text.to_string(),
                tool_name: None,
                timestamp: timestamp.to_string(),
                session_id: session_id.to_string(),
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
            let target_str = if is_bash { "bash-output" } else { "tool-result" };

            if !targets.contains(target_str) {
                continue;
            }

            if let Some(text) = extract_tool_result_text(block) {
                out.push(ExtractedContent {
                    target: if is_bash { Target::BashOutput } else { Target::ToolResult },
                    text,
                    tool_name: Some(tool_name),
                    timestamp: timestamp.to_string(),
                    session_id: session_id.to_string(),
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
    targets: &std::collections::HashSet<String>,
    session_id: &str,
    timestamp: &str,
    out: &mut Vec<ExtractedContent>,
) {
    let content = match entry["message"]["content"].as_array() {
        Some(a) => a,
        None => return,
    };

    for block in content {
        if block["type"] == "text" && targets.contains("assistant") {
            if let Some(text) = block["text"].as_str() {
                out.push(ExtractedContent {
                    target: Target::Assistant,
                    text: text.to_string(),
                    tool_name: None,
                    timestamp: timestamp.to_string(),
                    session_id: session_id.to_string(),
                });
            }
        }

        if block["type"] == "tool_use" {
            let name = block["name"].as_str().unwrap_or("").to_string();
            let input = &block["input"];

            if name == "Bash" && targets.contains("bash-command") {
                if let Some(cmd) = input["command"].as_str() {
                    out.push(ExtractedContent {
                        target: Target::BashCommand,
                        text: cmd.to_string(),
                        tool_name: Some("Bash".to_string()),
                        timestamp: timestamp.to_string(),
                        session_id: session_id.to_string(),
                    });
                }
            }

            if targets.contains("tool-use") && !name.is_empty() {
                out.push(ExtractedContent {
                    target: Target::ToolUse,
                    text: format_tool_input(input),
                    tool_name: Some(name),
                    timestamp: timestamp.to_string(),
                    session_id: session_id.to_string(),
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

    fn all_targets() -> HashSet<String> {
        ["user", "assistant", "bash-command", "bash-output", "tool-use", "tool-result", "subagent-prompt", "compact-summary"]
            .iter().map(|s| s.to_string()).collect()
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
        let user_only: HashSet<String> = ["user"].iter().map(|s| s.to_string()).collect();
        let contents = extract_content(f.path(), &user_only, "s", false);
        assert_eq!(contents.len(), 1);
        assert_eq!(contents[0].target, Target::User);
    }
}
