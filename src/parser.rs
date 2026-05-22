use std::fs;
use std::fmt;
use std::io::{BufRead, BufReader};
use std::path::Path;
use std::str::FromStr;
use std::collections::{HashMap, HashSet};

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum Target {
    User,
    Assistant,
    Thinking,
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
    AiTitle,
    PermissionMode,
    Attachment,
    Progress,
    PullRequest,
    BridgeSession,
}

/// CLI/display name for a target. Paired with `FromStr` below — these two
/// impls are the single source of truth for the target ↔ string mapping.
impl fmt::Display for Target {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Target::User => "user",
            Target::Assistant => "assistant",
            Target::Thinking => "thinking",
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
            Target::AiTitle => "ai-title",
            Target::PermissionMode => "permission-mode",
            Target::Attachment => "attachment",
            Target::Progress => "progress",
            Target::PullRequest => "pull-request",
            Target::BridgeSession => "bridge-session",
        })
    }
}

impl FromStr for Target {
    type Err = ();
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Ok(match s {
            "user" => Target::User,
            "assistant" => Target::Assistant,
            "thinking" => Target::Thinking,
            "bash-command" => Target::BashCommand,
            "bash-output" => Target::BashOutput,
            "tool-use" => Target::ToolUse,
            "tool-result" => Target::ToolResult,
            "subagent-prompt" => Target::SubagentPrompt,
            "compact-summary" => Target::CompactSummary,
            "system" => Target::System,
            "file-history-snapshot" => Target::FileHistorySnapshot,
            "queue-operation" => Target::QueueOperation,
            "last-prompt" => Target::LastPrompt,
            "agent-name" => Target::AgentName,
            "custom-title" => Target::CustomTitle,
            "ai-title" => Target::AiTitle,
            "permission-mode" => Target::PermissionMode,
            "attachment" => Target::Attachment,
            "progress" => Target::Progress,
            "pull-request" => Target::PullRequest,
            "bridge-session" => Target::BridgeSession,
            _ => return Err(()),
        })
    }
}

impl Target {
    /// Whether this target carries a subtype that can appear after `.` in the
    /// `--targets` flag. True iff some extraction site sets `tool_name: Some(_)`
    /// for this target.
    pub fn supports_subtypes(&self) -> bool {
        match self {
            Target::System
            | Target::Progress
            | Target::Attachment
            | Target::ToolUse
            | Target::ToolResult
            | Target::BashOutput
            | Target::BashCommand
            | Target::QueueOperation
            | Target::PullRequest => true,

            Target::User
            | Target::Assistant
            | Target::Thinking
            | Target::SubagentPrompt
            | Target::CompactSummary
            | Target::FileHistorySnapshot
            | Target::LastPrompt
            | Target::AgentName
            | Target::CustomTitle
            | Target::AiTitle
            | Target::PermissionMode
            | Target::BridgeSession => false,
        }
    }
}

/// A target plus an optional subtype qualifier — the unit of `--targets`
/// composition (`system`, `system.away_summary`, `tool-use.Edit`) and also
/// the unit of label rendering in dump/tail/last/search output.
///
/// `Display` and `FromStr` are the canonical encoders/decoders; both use the
/// `target.subtype` form so input and output round-trip.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct QualifiedTarget {
    pub target: Target,
    pub subtype: Option<String>,
}

impl QualifiedTarget {
    pub fn bare(target: Target) -> Self {
        Self { target, subtype: None }
    }
}

impl fmt::Display for QualifiedTarget {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.subtype {
            None => write!(f, "{}", self.target),
            Some(s) => write!(f, "{}.{}", self.target, s),
        }
    }
}

impl FromStr for QualifiedTarget {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let (name, subtype) = match s.split_once('.') {
            Some((n, sub)) => (n, Some(sub.to_string())),
            None => (s, None),
        };
        let target: Target = name.parse()
            .map_err(|_| format!("unknown target '{}'", name))?;
        if let Some(ref sub) = subtype {
            if !target.supports_subtypes() {
                return Err(format!(
                    "target '{}' does not have subtypes; ignoring qualifier '.{}'",
                    name, sub
                ));
            }
        }
        Ok(QualifiedTarget { target, subtype })
    }
}

/// Filter built from a `--targets` flag value.
///
/// Combines a set of bare targets with optional per-target subtype allow-lists.
/// A target with an entry in `subtype_filters` only matches records whose
/// `tool_name` is in the allow-list. A target without an entry matches every
/// record of that target.
#[derive(Clone, Debug, Default)]
pub struct TargetSelector {
    pub targets: HashSet<Target>,
    pub subtype_filters: HashMap<Target, HashSet<String>>,
}

impl TargetSelector {
    pub fn new() -> Self {
        Self::default()
    }

    /// Test whether a record passes the selector.
    pub fn matches(&self, target: &Target, tool_name: Option<&str>) -> bool {
        if !self.targets.contains(target) {
            return false;
        }
        match self.subtype_filters.get(target) {
            None => true,
            Some(allowed) => match tool_name {
                Some(s) => allowed.contains(s),
                None => false,
            },
        }
    }

}

impl FromIterator<QualifiedTarget> for TargetSelector {
    /// Build a selector from a stream of qualified targets. Bare entries
    /// (`subtype: None`) win over qualified ones (`subtype: Some(_)`) for the
    /// same target — so `[System.away_summary, System]` yields a selector
    /// with no subtype filter on System.
    fn from_iter<I: IntoIterator<Item = QualifiedTarget>>(iter: I) -> Self {
        let mut sel = TargetSelector::new();
        let mut bare: HashSet<Target> = HashSet::new();
        let mut qualified: Vec<(Target, String)> = vec![];

        for q in iter {
            sel.targets.insert(q.target.clone());
            match q.subtype {
                None => { bare.insert(q.target); }
                Some(s) => qualified.push((q.target, s)),
            }
        }

        for (t, s) in qualified {
            if bare.contains(&t) {
                continue;
            }
            sel.subtype_filters.entry(t).or_default().insert(s);
        }

        sel
    }
}

/// Diff data extracted from an Edit tool call.
#[derive(Clone)]
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
            extract_progress(entry, tool_use_map, targets, session_id, entry_session, &timestamp, is_subagent, out);
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
        Some("ai-title") => {
            if targets.contains(&Target::AiTitle) {
                let text = entry["aiTitle"].as_str().unwrap_or("");
                out.push(ExtractedContent {
                    target: Target::AiTitle,
                    text: text.to_string(),
                    tool_name: None,
                    timestamp: timestamp.to_string(),
                    session_id: entry_session.to_string(),
                    edit_diff: None,
                    raw_entry: None,
                });
            }
        }
        Some("permission-mode") => {
            if targets.contains(&Target::PermissionMode) {
                let mode = entry["permissionMode"].as_str().unwrap_or("");
                out.push(ExtractedContent {
                    target: Target::PermissionMode,
                    text: mode.to_string(),
                    tool_name: None,
                    timestamp: timestamp.to_string(),
                    session_id: entry_session.to_string(),
                    edit_diff: None,
                    raw_entry: None,
                });
            }
        }
        Some("bridge-session") => {
            // Linkage metadata written when a local session is mirrored to a
            // cloud "bridge" session — carries the bridge session id and the
            // last synced sequence number, no human-authored text.
            if targets.contains(&Target::BridgeSession) {
                let bridge_id = entry["bridgeSessionId"].as_str().unwrap_or("");
                out.push(ExtractedContent {
                    target: Target::BridgeSession,
                    text: bridge_id.to_string(),
                    tool_name: None,
                    timestamp: timestamp.to_string(),
                    session_id: entry_session.to_string(),
                    edit_diff: None,
                    raw_entry: None,
                });
            }
        }
        Some("attachment") => {
            if targets.contains(&Target::Attachment) {
                if let Some(text) = format_attachment(&entry["attachment"]) {
                    let inner_type = entry["attachment"]["type"].as_str().map(String::from);
                    out.push(ExtractedContent {
                        target: Target::Attachment,
                        text,
                        tool_name: inner_type,
                        timestamp: timestamp.to_string(),
                        session_id: entry_session.to_string(),
                        edit_diff: None,
                        raw_entry: None,
                    });
                }
            }
        }
        _ => {
            // Pull-request metadata records have no `type` field — they surface
            // a PR number / owning repo / URL alongside a sessionId, written
            // when a session is associated with a GitHub PR.
            if entry.get("prRepository").and_then(|v| v.as_str()).is_some() {
                if targets.contains(&Target::PullRequest) {
                    let repo = entry["prRepository"].as_str().unwrap_or("");
                    let number = entry["prNumber"].as_u64();
                    let url = entry["prUrl"].as_str().unwrap_or("");
                    let text = match (number, url.is_empty()) {
                        (Some(n), false) => format!("{}#{}\n{}", repo, n, url),
                        (Some(n), true) => format!("{}#{}", repo, n),
                        (None, false) => format!("{}\n{}", repo, url),
                        (None, true) => repo.to_string(),
                    };
                    out.push(ExtractedContent {
                        target: Target::PullRequest,
                        text,
                        tool_name: Some(repo.to_string()),
                        timestamp: timestamp.to_string(),
                        session_id: entry_session.to_string(),
                        edit_diff: None,
                        raw_entry: None,
                    });
                }
                return;
            }

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

    if let Some(text) = content.as_str() {
        if let Some(target) = user_target {
            out.push(ExtractedContent {
                target,
                text: text.to_string(),
                tool_name: None,
                timestamp: timestamp.to_string(),
                session_id: session_id.to_string(),
                edit_diff: None,
                raw_entry: None,
            });
        }
        return;
    }

    let arr = match content.as_array() {
        Some(a) => a,
        None => return,
    };

    for block in arr {
        let blk_type = block["type"].as_str().unwrap_or("");
        match blk_type {
            "text" => {
                if let Some(target) = user_target.clone() {
                    if let Some(text) = block["text"].as_str() {
                        out.push(ExtractedContent {
                            target,
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
            "tool_result" => {
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
            "image" => {
                if let Some(target) = user_target.clone() {
                    out.push(ExtractedContent {
                        target,
                        text: format_image_marker(&block["source"]),
                        tool_name: None,
                        timestamp: timestamp.to_string(),
                        session_id: session_id.to_string(),
                        edit_diff: None,
                        raw_entry: None,
                    });
                }
            }
            "document" => {
                if let Some(target) = user_target.clone() {
                    out.push(ExtractedContent {
                        target,
                        text: format_document_marker(block),
                        tool_name: None,
                        timestamp: timestamp.to_string(),
                        session_id: session_id.to_string(),
                        edit_diff: None,
                        raw_entry: None,
                    });
                }
            }
            other => {
                eprintln!("warning: skipping unrecognized user content block type '{}'", other);
            }
        }
    }
}

/// Render an attachment record's payload as searchable text.
///
/// Dispatches on the attachment's inner `type`.  Unknown subtypes emit a
/// warning and fall back to the raw JSON so nothing is silently dropped.
fn format_attachment(attachment: &serde_json::Value) -> Option<String> {
    let subtype = attachment["type"].as_str().unwrap_or("");
    match subtype {
        "deferred_tools_delta" | "mcp_server_delta" => format_attachment_delta(attachment),
        "task_reminder" | "skill_listing" => {
            attachment["content"].as_str().filter(|s| !s.is_empty()).map(String::from)
        }
        "queued_command" => {
            attachment["prompt"].as_str().filter(|s| !s.is_empty()).map(String::from)
        }
        "edited_text_file" => {
            let filename = attachment["filename"].as_str().unwrap_or("");
            let snippet = attachment["snippet"].as_str().unwrap_or("");
            if filename.is_empty() && snippet.is_empty() { None }
            else if snippet.is_empty() { Some(filename.to_string()) }
            else { Some(format!("{}\n{}", filename, snippet)) }
        }
        "file" => {
            let filename = attachment["filename"].as_str()
                .or_else(|| attachment["displayPath"].as_str())
                .unwrap_or("");
            let content = attachment["content"].as_str().unwrap_or("");
            if filename.is_empty() && content.is_empty() { None }
            else if content.is_empty() { Some(filename.to_string()) }
            else { Some(format!("{}\n{}", filename, content)) }
        }
        "hook_success" | "hook_cancelled" => {
            let hook_name = attachment["hookName"].as_str().unwrap_or("");
            let command = attachment["command"].as_str().unwrap_or("");
            let stdout = attachment["stdout"].as_str().unwrap_or("");
            let stderr = attachment["stderr"].as_str().unwrap_or("");
            let mut lines = vec![];
            if !hook_name.is_empty() || !command.is_empty() {
                lines.push(format!("{}: {}", hook_name, command));
            }
            if !stdout.is_empty() { lines.push(stdout.to_string()); }
            if !stderr.is_empty() { lines.push(stderr.to_string()); }
            if lines.is_empty() { None } else { Some(lines.join("\n")) }
        }
        "date_change" => {
            attachment["newDate"].as_str().filter(|s| !s.is_empty()).map(String::from)
        }
        "compact_file_reference" => {
            attachment["displayPath"].as_str()
                .or_else(|| attachment["filename"].as_str())
                .filter(|s| !s.is_empty())
                .map(String::from)
        }
        _ => {
            eprintln!("warning: skipping unrecognized attachment subtype '{}'", subtype);
            Some(attachment.to_string())
        }
    }
}

/// Format a `deferred_tools_delta` or `mcp_server_delta` attachment.
///
/// Returns `None` when the delta has nothing added or removed.
fn format_attachment_delta(attachment: &serde_json::Value) -> Option<String> {
    let collect_strs = |arr: Option<&Vec<serde_json::Value>>| -> Vec<String> {
        arr.map(|a| a.iter().filter_map(|v| v.as_str().map(String::from)).collect())
            .unwrap_or_default()
    };

    let added = collect_strs(attachment["addedNames"].as_array());
    let removed = collect_strs(attachment["removedNames"].as_array());
    let blocks = collect_strs(attachment["addedBlocks"].as_array());

    if added.is_empty() && removed.is_empty() && blocks.is_empty() {
        return None;
    }

    let mut lines = vec![];
    if !added.is_empty() {
        lines.push(format!("+ {}", added.join(", ")));
    }
    if !removed.is_empty() {
        lines.push(format!("- {}", removed.join(", ")));
    }
    if !blocks.is_empty() {
        lines.push(blocks.join("\n"));
    }
    Some(lines.join("\n"))
}

/// Decoded byte count of a base64 string without actually decoding it. Base64
/// expands 3 bytes into 4 chars; trailing `=` padding tells us how many of the
/// final 3 bytes are real.
fn base64_decoded_len(data: &str) -> usize {
    let padding = data.bytes().rev().take_while(|&b| b == b'=').count();
    let upper = data.len() / 4 * 3;
    upper - padding.min(upper)
}

/// Render an image content block as a short `[image: <mime>, <N> bytes]` marker so
/// searches over user messages at least see that an image was present, even though
/// the pixel data itself isn't searchable. For URL-sourced images, surface the URL
/// instead of byte count.
fn format_image_marker(source: &serde_json::Value) -> String {
    let source_type = source["type"].as_str().unwrap_or("");
    let media_type = source["media_type"].as_str().unwrap_or("image");
    match source_type {
        "base64" => {
            let data = source["data"].as_str().unwrap_or("");
            format!("[image: {}, {} bytes]", media_type, base64_decoded_len(data))
        }
        "url" => {
            let url = source["url"].as_str().unwrap_or("");
            if url.is_empty() {
                format!("[image: {}]", media_type)
            } else {
                format!("[image: {}, {}]", media_type, url)
            }
        }
        _ => format!("[image: {}]", media_type),
    }
}

/// Render a document content block (e.g. a PDF attached to a user message) as a
/// short `[document: <mime>, <N> bytes]` marker — the binary payload itself
/// isn't searchable, but searches should at least see that an attachment was
/// present, and an optional `title` is surfaced so a search for the filename
/// can match. Mirrors the image marker's URL handling but covers the extra
/// `text` and `file` source variants the Anthropic API allows for documents.
fn format_document_marker(block: &serde_json::Value) -> String {
    let source = &block["source"];
    let source_type = source["type"].as_str().unwrap_or("");
    let media_type = source["media_type"].as_str().unwrap_or("document");
    let detail = match source_type {
        "base64" => {
            let data = source["data"].as_str().unwrap_or("");
            format!("[document: {}, {} bytes]", media_type, base64_decoded_len(data))
        }
        "text" => {
            let text = source["data"].as_str().unwrap_or("");
            format!("[document: {}, {} bytes]", media_type, text.len())
        }
        "url" => {
            let url = source["url"].as_str().unwrap_or("");
            if url.is_empty() {
                format!("[document: {}]", media_type)
            } else {
                format!("[document: {}, {}]", media_type, url)
            }
        }
        "file" => {
            let file_id = source["file_id"].as_str().unwrap_or("");
            if file_id.is_empty() {
                format!("[document: {}]", media_type)
            } else {
                format!("[document: {}, {}]", media_type, file_id)
            }
        }
        _ => format!("[document: {}]", media_type),
    };
    match block["title"].as_str().filter(|s| !s.is_empty()) {
        Some(title) => format!("{} {}", detail, title),
        None => detail,
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

fn extract_progress(
    entry: &serde_json::Value,
    tool_use_map: &ToolUseMap,
    targets: &std::collections::HashSet<Target>,
    session_id: &str,
    entry_session: &str,
    timestamp: &str,
    is_subagent: bool,
    out: &mut Vec<ExtractedContent>,
) {
    let data = &entry["data"];
    let subtype = data["type"].as_str().unwrap_or("");

    // agent_progress wraps a full nested assistant/user message — synthesize
    // a top-level entry so all normal extraction rules (text, thinking,
    // tool_use, tool_result) apply to it.
    if subtype == "agent_progress" {
        if let Some(inner_msg) = data["message"]["message"].as_object() {
            let nested_type = data["message"]["type"].as_str();
            let synth_type = if nested_type == Some("assistant") { "assistant" } else { "user" };
            let synth = serde_json::json!({
                "type": synth_type,
                "message": inner_msg,
                "timestamp": timestamp,
                "sessionId": entry_session,
            });
            extract_from_entry(&synth, tool_use_map, targets, session_id, is_subagent, out);
        }
        return;
    }

    if !targets.contains(&Target::Progress) {
        return;
    }

    let text = match subtype {
        "hook_progress" => {
            let hook_name = data["hookName"].as_str().unwrap_or("");
            let command = data["command"].as_str().unwrap_or("");
            format!("{}: {}", hook_name, command)
        }
        "bash_progress" => {
            data["fullOutput"].as_str()
                .or_else(|| data["output"].as_str())
                .unwrap_or("").to_string()
        }
        "query_update" => {
            data["query"].as_str().unwrap_or("").to_string()
        }
        "search_results_received" => {
            let query = data["query"].as_str().unwrap_or("");
            let count = data["resultCount"].as_u64().unwrap_or(0);
            format!("{} ({} results)", query, count)
        }
        "waiting_for_task" => {
            let desc = data["taskDescription"].as_str().unwrap_or("");
            let task_type = data["taskType"].as_str().unwrap_or("");
            format!("{}: {}", task_type, desc)
        }
        other => {
            eprintln!("warning: skipping unrecognized progress subtype '{}'", other);
            data.to_string()
        }
    };

    out.push(ExtractedContent {
        target: Target::Progress,
        text,
        tool_name: Some(subtype.to_string()),
        timestamp: timestamp.to_string(),
        session_id: entry_session.to_string(),
        edit_diff: None,
        raw_entry: None,
    });
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
        let blk_type = block["type"].as_str().unwrap_or("");
        match blk_type {
            "text" | "thinking" | "tool_use" => {}
            other => {
                eprintln!("warning: skipping unrecognized assistant content block type '{}'", other);
            }
        }

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

        if block["type"] == "thinking" && targets.contains(&Target::Thinking) {
            if let Some(text) = block["thinking"].as_str() {
                out.push(ExtractedContent {
                    target: Target::Thinking,
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
            Target::User, Target::Assistant, Target::Thinking, Target::BashCommand, Target::BashOutput,
            Target::ToolUse, Target::ToolResult, Target::SubagentPrompt, Target::CompactSummary,
            Target::System, Target::FileHistorySnapshot, Target::QueueOperation,
            Target::LastPrompt, Target::AgentName, Target::CustomTitle, Target::AiTitle,
            Target::PermissionMode, Target::Attachment, Target::Progress,
            Target::PullRequest, Target::BridgeSession,
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
    fn test_extract_assistant_thinking() {
        let f = write_jsonl(&[
            r#"{"type":"assistant","message":{"role":"assistant","content":[{"type":"thinking","thinking":"pondering"},{"type":"text","text":"answer"}]},"timestamp":"2024-01-01T00:00:00Z","sessionId":"s"}"#,
        ]);
        let contents = extract_content(f.path(), &all_targets(), "s", false);
        let thinking = contents.iter().find(|c| c.target == Target::Thinking);
        assert!(thinking.is_some());
        assert_eq!(thinking.unwrap().text, "pondering");
        let text = contents.iter().find(|c| c.target == Target::Assistant);
        assert_eq!(text.unwrap().text, "answer");
    }

    #[test]
    fn test_thinking_target_filter_excludes_thinking() {
        let f = write_jsonl(&[
            r#"{"type":"assistant","message":{"role":"assistant","content":[{"type":"thinking","thinking":"pondering"},{"type":"text","text":"answer"}]},"timestamp":"2024-01-01T00:00:00Z","sessionId":"s"}"#,
        ]);
        let targets: HashSet<Target> = [Target::Assistant].into_iter().collect();
        let contents = extract_content(f.path(), &targets, "s", false);
        assert_eq!(contents.len(), 1);
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

    #[test]
    fn test_extract_ai_title() {
        let f = write_jsonl(&[
            r#"{"type":"ai-title","aiTitle":"Some autogenerated title","sessionId":"s"}"#,
        ]);
        let contents = extract_content(f.path(), &all_targets(), "s", false);
        let at = contents.iter().find(|c| c.target == Target::AiTitle);
        assert!(at.is_some(), "should extract ai-title record");
        assert_eq!(at.unwrap().text, "Some autogenerated title");
    }

    #[test]
    fn test_ai_title_not_warned_as_unrecognized() {
        let f = write_jsonl(&[
            r#"{"type":"ai-title","aiTitle":"test","sessionId":"s"}"#,
        ]);
        let targets: HashSet<Target> = [Target::AiTitle].into_iter().collect();
        let contents = extract_content(f.path(), &targets, "s", false);
        assert_eq!(contents.len(), 1);
    }

    #[test]
    fn test_extract_permission_mode() {
        let f = write_jsonl(&[
            r#"{"type":"permission-mode","permissionMode":"bypassPermissions","sessionId":"s"}"#,
        ]);
        let contents = extract_content(f.path(), &all_targets(), "s", false);
        let pm = contents.iter().find(|c| c.target == Target::PermissionMode);
        assert!(pm.is_some(), "should extract permission-mode record");
        assert_eq!(pm.unwrap().text, "bypassPermissions");
    }

    #[test]
    fn test_permission_mode_target_filtering() {
        // Without PermissionMode in targets, the record should be silently skipped.
        let f = write_jsonl(&[
            r#"{"type":"permission-mode","permissionMode":"plan","sessionId":"s"}"#,
        ]);
        let targets: HashSet<Target> = [Target::User].into_iter().collect();
        let contents = extract_content(f.path(), &targets, "s", false);
        assert_eq!(contents.len(), 0);
    }

    #[test]
    fn test_extract_attachment_deferred_tools_delta() {
        let f = write_jsonl(&[
            r#"{"type":"attachment","attachment":{"type":"deferred_tools_delta","addedNames":["WebFetch","WebSearch"],"addedLines":["WebFetch","WebSearch"],"removedNames":["OldTool"]},"sessionId":"s"}"#,
        ]);
        let contents = extract_content(f.path(), &all_targets(), "s", false);
        let att = contents.iter().find(|c| c.target == Target::Attachment);
        assert!(att.is_some(), "should extract attachment record");
        let att = att.unwrap();
        assert!(att.text.contains("WebFetch"));
        assert!(att.text.contains("WebSearch"));
        assert!(att.text.contains("OldTool"));
        assert_eq!(att.tool_name.as_deref(), Some("deferred_tools_delta"));
    }

    #[test]
    fn test_extract_attachment_mcp_server_delta_with_blocks() {
        // mcp_server_delta records contain human-readable `addedBlocks` describing
        // added MCP servers — this is the most useful searchable content.
        let line = "{\"type\":\"attachment\",\"attachment\":{\"type\":\"mcp_server_delta\",\
            \"addedNames\":[\"Lever MCP\"],\
            \"addedBlocks\":[\"## Lever MCP\\nRecruiting tools for Lever\"],\
            \"removedNames\":[]},\"sessionId\":\"s\"}";
        let f = write_jsonl(&[line]);
        let contents = extract_content(f.path(), &all_targets(), "s", false);
        let att = contents.iter().find(|c| c.target == Target::Attachment).unwrap();
        assert!(att.text.contains("Lever MCP"));
        assert!(att.text.contains("Recruiting tools"),
            "attachment text should include addedBlocks prose for searching");
    }

    #[test]
    fn test_extract_attachment_empty_payload_is_skipped() {
        // An attachment with nothing added or removed produces no ExtractedContent.
        let f = write_jsonl(&[
            r#"{"type":"attachment","attachment":{"type":"deferred_tools_delta","addedNames":[],"addedLines":[],"removedNames":[]},"sessionId":"s"}"#,
        ]);
        let contents = extract_content(f.path(), &all_targets(), "s", false);
        assert_eq!(contents.len(), 0, "empty attachment should be skipped, not extracted");
    }

    #[test]
    fn test_attachment_target_filtering() {
        let f = write_jsonl(&[
            r#"{"type":"attachment","attachment":{"type":"deferred_tools_delta","addedNames":["WebFetch"],"addedLines":["WebFetch"],"removedNames":[]},"sessionId":"s"}"#,
        ]);
        let targets: HashSet<Target> = [Target::User].into_iter().collect();
        let contents = extract_content(f.path(), &targets, "s", false);
        assert_eq!(contents.len(), 0);
    }

    #[test]
    fn test_extract_attachment_task_reminder() {
        let f = write_jsonl(&[
            r#"{"type":"attachment","attachment":{"type":"task_reminder","content":"don't forget the thing","itemCount":1},"sessionId":"s"}"#,
        ]);
        let contents = extract_content(f.path(), &all_targets(), "s", false);
        let att = contents.iter().find(|c| c.target == Target::Attachment).unwrap();
        assert_eq!(att.text, "don't forget the thing");
        assert_eq!(att.tool_name.as_deref(), Some("task_reminder"));
    }

    #[test]
    fn test_extract_attachment_queued_command() {
        let f = write_jsonl(&[
            r#"{"type":"attachment","attachment":{"type":"queued_command","prompt":"run tests","commandMode":"prompt"},"sessionId":"s"}"#,
        ]);
        let contents = extract_content(f.path(), &all_targets(), "s", false);
        let att = contents.iter().find(|c| c.target == Target::Attachment).unwrap();
        assert_eq!(att.text, "run tests");
    }

    #[test]
    fn test_extract_attachment_edited_text_file() {
        let f = write_jsonl(&[
            r#"{"type":"attachment","attachment":{"type":"edited_text_file","filename":"src/foo.rs","snippet":"fn bar() {}"},"sessionId":"s"}"#,
        ]);
        let contents = extract_content(f.path(), &all_targets(), "s", false);
        let att = contents.iter().find(|c| c.target == Target::Attachment).unwrap();
        assert!(att.text.contains("src/foo.rs"));
        assert!(att.text.contains("fn bar() {}"));
    }

    #[test]
    fn test_extract_attachment_file() {
        let f = write_jsonl(&[
            r#"{"type":"attachment","attachment":{"type":"file","filename":"README.md","displayPath":"./README.md","content":"hello"},"sessionId":"s"}"#,
        ]);
        let contents = extract_content(f.path(), &all_targets(), "s", false);
        let att = contents.iter().find(|c| c.target == Target::Attachment).unwrap();
        assert!(att.text.contains("README.md"));
        assert!(att.text.contains("hello"));
    }

    #[test]
    fn test_extract_attachment_hook_success() {
        let f = write_jsonl(&[
            r#"{"type":"attachment","attachment":{"type":"hook_success","hookName":"pre-tool","command":"lint","stdout":"ok","stderr":"","exitCode":0,"durationMs":10,"hookEvent":"PreToolUse","toolUseID":"tu"},"sessionId":"s"}"#,
        ]);
        let contents = extract_content(f.path(), &all_targets(), "s", false);
        let att = contents.iter().find(|c| c.target == Target::Attachment).unwrap();
        assert!(att.text.contains("pre-tool"));
        assert!(att.text.contains("lint"));
        assert!(att.text.contains("ok"));
    }

    #[test]
    fn test_extract_attachment_date_change() {
        let f = write_jsonl(&[
            r#"{"type":"attachment","attachment":{"type":"date_change","newDate":"2026-04-15"},"sessionId":"s"}"#,
        ]);
        let contents = extract_content(f.path(), &all_targets(), "s", false);
        let att = contents.iter().find(|c| c.target == Target::Attachment).unwrap();
        assert_eq!(att.text, "2026-04-15");
    }

    #[test]
    fn test_extract_attachment_compact_file_reference() {
        let f = write_jsonl(&[
            r#"{"type":"attachment","attachment":{"type":"compact_file_reference","displayPath":"./foo.rs","filename":"foo.rs"},"sessionId":"s"}"#,
        ]);
        let contents = extract_content(f.path(), &all_targets(), "s", false);
        let att = contents.iter().find(|c| c.target == Target::Attachment).unwrap();
        assert_eq!(att.text, "./foo.rs");
    }

    #[test]
    fn test_unknown_attachment_subtype_is_not_silently_dropped() {
        let f = write_jsonl(&[
            r#"{"type":"attachment","attachment":{"type":"totally_new_subtype","payload":"UNIQUE_UNKNOWN_ATT"},"sessionId":"s"}"#,
        ]);
        let contents = extract_content(f.path(), &all_targets(), "s", false);
        let att = contents.iter().find(|c| c.target == Target::Attachment)
            .expect("unknown attachment subtype should still produce an ExtractedContent");
        assert!(att.text.contains("UNIQUE_UNKNOWN_ATT"),
            "unknown attachment payload should be preserved in output");
    }

    #[test]
    fn test_unknown_assistant_block_type_is_not_silently_dropped() {
        // Known block types in assistant.content are text, thinking, tool_use.
        // An unrecognized subtype must not crash extraction and should let other
        // blocks still be extracted.
        let f = write_jsonl(&[
            r#"{"type":"assistant","message":{"role":"assistant","content":[{"type":"new_future_block","data":"x"},{"type":"text","text":"hello"}]},"timestamp":"2024-01-01T00:00:00Z","sessionId":"s"}"#,
        ]);
        let contents = extract_content(f.path(), &all_targets(), "s", false);
        let txt = contents.iter().find(|c| c.target == Target::Assistant).unwrap();
        assert_eq!(txt.text, "hello");
    }

    #[test]
    fn test_unknown_user_block_type_is_not_silently_dropped() {
        let f = write_jsonl(&[
            r#"{"type":"user","message":{"role":"user","content":[{"type":"new_future_block","data":"x"},{"type":"text","text":"hi"}]},"timestamp":"2024-01-01T00:00:00Z","sessionId":"s"}"#,
        ]);
        let contents = extract_content(f.path(), &all_targets(), "s", false);
        let txt = contents.iter().find(|c| c.target == Target::User).unwrap();
        assert_eq!(txt.text, "hi");
    }

    #[test]
    fn test_image_user_block_renders_placeholder_marker() {
        // Images should produce a short `[image: <mime>, <N> bytes]` marker rather
        // than triggering the "unrecognized block" warning. A sibling text block
        // in the same message must still be extracted independently.
        // The data "AAAA" is 4 base64 chars = 3 decoded bytes.
        let f = write_jsonl(&[
            r#"{"type":"user","message":{"role":"user","content":[{"type":"image","source":{"type":"base64","media_type":"image/png","data":"AAAA"}},{"type":"text","text":"caption"}]},"timestamp":"2024-01-01T00:00:00Z","sessionId":"s"}"#,
        ]);
        let contents = extract_content(f.path(), &all_targets(), "s", false);
        let user_blocks: Vec<_> = contents.iter().filter(|c| c.target == Target::User).collect();
        assert_eq!(user_blocks.len(), 2, "both image marker and text should be extracted");
        assert_eq!(user_blocks[0].text, "[image: image/png, 3 bytes]");
        assert_eq!(user_blocks[1].text, "caption");
    }

    #[test]
    fn test_image_url_source_renders_url_marker() {
        let f = write_jsonl(&[
            r#"{"type":"user","message":{"role":"user","content":[{"type":"image","source":{"type":"url","media_type":"image/jpeg","url":"https://example.com/pic.jpg"}}]},"timestamp":"2024-01-01T00:00:00Z","sessionId":"s"}"#,
        ]);
        let contents = extract_content(f.path(), &all_targets(), "s", false);
        let user = contents.iter().find(|c| c.target == Target::User).unwrap();
        assert_eq!(user.text, "[image: image/jpeg, https://example.com/pic.jpg]");
    }

    #[test]
    fn test_document_user_block_renders_placeholder_marker() {
        // Documents (e.g. PDFs attached to a user message) should produce a short
        // `[document: <mime>, <N> bytes]` marker rather than triggering the
        // "unrecognized block" warning. Sibling text blocks must still extract.
        // "AAAA" is 4 base64 chars = 3 decoded bytes.
        let f = write_jsonl(&[
            r#"{"type":"user","message":{"role":"user","content":[{"type":"document","source":{"type":"base64","media_type":"application/pdf","data":"AAAA"}},{"type":"text","text":"see attached"}]},"timestamp":"2024-01-01T00:00:00Z","sessionId":"s"}"#,
        ]);
        let contents = extract_content(f.path(), &all_targets(), "s", false);
        let user_blocks: Vec<_> = contents.iter().filter(|c| c.target == Target::User).collect();
        assert_eq!(user_blocks.len(), 2, "both document marker and text should be extracted");
        assert_eq!(user_blocks[0].text, "[document: application/pdf, 3 bytes]");
        assert_eq!(user_blocks[1].text, "see attached");
    }

    #[test]
    fn test_document_url_source_renders_url_marker() {
        let f = write_jsonl(&[
            r#"{"type":"user","message":{"role":"user","content":[{"type":"document","source":{"type":"url","media_type":"application/pdf","url":"https://example.com/paper.pdf"}}]},"timestamp":"2024-01-01T00:00:00Z","sessionId":"s"}"#,
        ]);
        let contents = extract_content(f.path(), &all_targets(), "s", false);
        let user = contents.iter().find(|c| c.target == Target::User).unwrap();
        assert_eq!(user.text, "[document: application/pdf, https://example.com/paper.pdf]");
    }

    #[test]
    fn test_document_title_appended_to_marker() {
        // A document's optional `title` (often the filename) should be surfaced
        // so searches for the filename can match.
        let f = write_jsonl(&[
            r#"{"type":"user","message":{"role":"user","content":[{"type":"document","source":{"type":"base64","media_type":"application/pdf","data":"AAAA"},"title":"resume.pdf"}]},"timestamp":"2024-01-01T00:00:00Z","sessionId":"s"}"#,
        ]);
        let contents = extract_content(f.path(), &all_targets(), "s", false);
        let user = contents.iter().find(|c| c.target == Target::User).unwrap();
        assert_eq!(user.text, "[document: application/pdf, 3 bytes] resume.pdf");
    }

    #[test]
    fn test_extract_pull_request_record() {
        // PR records have no `type` field — they carry prNumber/prRepository/prUrl
        // and a sessionId. They should be recognized as pull-request target rather
        // than hitting the unrecognized-record warning.
        let f = write_jsonl(&[
            r#"{"prNumber":13,"prRepository":"futpib/slopd","prUrl":"https://github.com/futpib/slopd/pull/13","sessionId":"s"}"#,
        ]);
        let contents = extract_content(f.path(), &all_targets(), "s", false);
        let pr = contents.iter().find(|c| c.target == Target::PullRequest)
            .expect("pull-request record should be extracted");
        assert!(pr.text.contains("futpib/slopd#13"), "text should include repo#number: {:?}", pr.text);
        assert!(pr.text.contains("https://github.com/futpib/slopd/pull/13"),
            "text should include URL: {:?}", pr.text);
        assert_eq!(pr.tool_name.as_deref(), Some("futpib/slopd"));
    }

    #[test]
    fn test_pull_request_target_filtering() {
        // Without PullRequest in targets, the record should be silently skipped
        // (no warning, no extracted content).
        let f = write_jsonl(&[
            r#"{"prNumber":2,"prRepository":"futpib/goal","prUrl":"https://github.com/futpib/goal/pull/2","sessionId":"s"}"#,
        ]);
        let targets: HashSet<Target> = [Target::User].into_iter().collect();
        let contents = extract_content(f.path(), &targets, "s", false);
        assert_eq!(contents.len(), 0, "should not extract without PullRequest in target set");
    }

    #[test]
    fn test_pull_request_record_falls_back_without_number_or_url() {
        // Even partially-populated PR records should still be recognized — we
        // fall back to whatever fields are present rather than warning.
        let f = write_jsonl(&[
            r#"{"prRepository":"owner/repo","sessionId":"s"}"#,
        ]);
        let contents = extract_content(f.path(), &all_targets(), "s", false);
        let pr = contents.iter().find(|c| c.target == Target::PullRequest)
            .expect("repo-only PR record should still be extracted");
        assert_eq!(pr.text, "owner/repo");
    }

    #[test]
    fn test_extract_progress_hook() {
        let f = write_jsonl(&[
            r#"{"type":"progress","data":{"type":"hook_progress","hookEvent":"PreToolUse","hookName":"PreToolUse:Bash","command":"/usr/bin/hook"},"timestamp":"2024-01-01T00:00:00Z","sessionId":"s"}"#,
        ]);
        let contents = extract_content(f.path(), &all_targets(), "s", false);
        let pg = contents.iter().find(|c| c.target == Target::Progress).unwrap();
        assert!(pg.text.contains("PreToolUse:Bash"));
        assert!(pg.text.contains("/usr/bin/hook"));
        assert_eq!(pg.tool_name.as_deref(), Some("hook_progress"));
    }

    #[test]
    fn test_extract_progress_bash() {
        let f = write_jsonl(&[
            r#"{"type":"progress","data":{"type":"bash_progress","fullOutput":"UNIQUE_BASH_OUT","output":"trunc","taskId":"x"},"timestamp":"2024-01-01T00:00:00Z","sessionId":"s"}"#,
        ]);
        let contents = extract_content(f.path(), &all_targets(), "s", false);
        let pg = contents.iter().find(|c| c.target == Target::Progress).unwrap();
        assert_eq!(pg.text, "UNIQUE_BASH_OUT");
    }

    #[test]
    fn test_extract_progress_agent_still_nests() {
        // agent_progress records wrap a nested assistant/user message — the extractor
        // must still recurse into them so the inner text/tool_use is captured.
        let f = write_jsonl(&[
            r#"{"type":"progress","data":{"type":"agent_progress","agentId":"a1","message":{"type":"assistant","message":{"role":"assistant","content":[{"type":"text","text":"NESTED_AGENT_MSG"}]}}},"timestamp":"2024-01-01T00:00:00Z","sessionId":"s"}"#,
        ]);
        let contents = extract_content(f.path(), &all_targets(), "s", false);
        let msg = contents.iter().find(|c| c.target == Target::Assistant).unwrap();
        assert_eq!(msg.text, "NESTED_AGENT_MSG");
    }

    #[test]
    fn test_unknown_progress_subtype_is_not_silently_dropped() {
        let f = write_jsonl(&[
            r#"{"type":"progress","data":{"type":"totally_new_progress","payload":"UNIQUE_UNKNOWN_PROG"},"timestamp":"2024-01-01T00:00:00Z","sessionId":"s"}"#,
        ]);
        let contents = extract_content(f.path(), &all_targets(), "s", false);
        let pg = contents.iter().find(|c| c.target == Target::Progress).unwrap();
        assert!(pg.text.contains("UNIQUE_UNKNOWN_PROG"));
    }

    #[test]
    fn test_target_selector_bare_matches_any_subtype() {
        let sel: TargetSelector = ["system".parse::<QualifiedTarget>().unwrap()]
            .into_iter().collect();
        assert!(sel.matches(&Target::System, Some("away_summary")));
        assert!(sel.matches(&Target::System, Some("api_error")));
        assert!(sel.matches(&Target::System, None));
        assert!(!sel.matches(&Target::Assistant, None));
    }

    #[test]
    fn test_target_selector_qualified_filters_by_subtype() {
        let sel: TargetSelector = [
            "system.away_summary".parse::<QualifiedTarget>().unwrap(),
            "system.api_error".parse::<QualifiedTarget>().unwrap(),
        ].into_iter().collect();
        assert!(sel.matches(&Target::System, Some("away_summary")));
        assert!(sel.matches(&Target::System, Some("api_error")));
        assert!(!sel.matches(&Target::System, Some("turn_duration")));
        assert!(!sel.matches(&Target::System, None));
    }

    #[test]
    fn test_target_selector_bare_wins_over_qualified() {
        let sel: TargetSelector = [
            "system.away_summary".parse::<QualifiedTarget>().unwrap(),
            "system".parse::<QualifiedTarget>().unwrap(),
        ].into_iter().collect();
        assert!(sel.matches(&Target::System, Some("away_summary")));
        assert!(sel.matches(&Target::System, Some("anything_else")));
        assert!(sel.matches(&Target::System, None));
    }

    #[test]
    fn test_target_selector_bare_wins_regardless_of_order() {
        let sel: TargetSelector = [
            "system".parse::<QualifiedTarget>().unwrap(),
            "system.away_summary".parse::<QualifiedTarget>().unwrap(),
        ].into_iter().collect();
        assert!(sel.matches(&Target::System, Some("anything_else")));
    }

    #[test]
    fn test_target_supports_subtypes() {
        assert!(Target::System.supports_subtypes());
        assert!(Target::ToolUse.supports_subtypes());
        assert!(Target::PullRequest.supports_subtypes());
        assert!(!Target::User.supports_subtypes());
        assert!(!Target::Assistant.supports_subtypes());
        assert!(!Target::CompactSummary.supports_subtypes());
    }

    #[test]
    fn test_target_display_fromstr_roundtrip() {
        for t in [
            Target::User, Target::Assistant, Target::Thinking,
            Target::BashCommand, Target::BashOutput, Target::ToolUse,
            Target::ToolResult, Target::SubagentPrompt, Target::CompactSummary,
            Target::System, Target::FileHistorySnapshot, Target::QueueOperation,
            Target::LastPrompt, Target::AgentName, Target::CustomTitle, Target::AiTitle,
            Target::PermissionMode, Target::Attachment, Target::Progress,
            Target::PullRequest, Target::BridgeSession,
        ] {
            let s = t.to_string();
            let parsed: Target = s.parse().unwrap_or_else(|_| panic!("failed to parse {}", s));
            assert_eq!(parsed, t);
        }
    }

    #[test]
    fn test_target_fromstr_unknown_errors() {
        assert!("totally-bogus".parse::<Target>().is_err());
        assert!("".parse::<Target>().is_err());
    }

    #[test]
    fn test_qualified_target_display_bare_and_qualified() {
        assert_eq!(QualifiedTarget::bare(Target::System).to_string(), "system");
        assert_eq!(
            QualifiedTarget { target: Target::System, subtype: Some("away_summary".into()) }.to_string(),
            "system.away_summary",
        );
        assert_eq!(
            QualifiedTarget { target: Target::ToolUse, subtype: Some("Edit".into()) }.to_string(),
            "tool-use.Edit",
        );
        // PR repo names contain '/', survives unchanged on the right of '.'
        assert_eq!(
            QualifiedTarget { target: Target::PullRequest, subtype: Some("owner/repo".into()) }.to_string(),
            "pull-request.owner/repo",
        );
    }

    #[test]
    fn test_qualified_target_fromstr_roundtrip() {
        for s in &[
            "system",
            "system.away_summary",
            "tool-use.Edit",
            "attachment.task_reminder",
            "pull-request.owner/repo",
        ] {
            let q: QualifiedTarget = s.parse().unwrap();
            assert_eq!(q.to_string(), *s);
        }
    }

    #[test]
    fn test_qualified_target_fromstr_rejects_subtype_on_typeless_target() {
        let err = "user.foo".parse::<QualifiedTarget>().unwrap_err();
        assert!(err.contains("does not have subtypes"), "got: {}", err);
    }

    #[test]
    fn test_qualified_target_fromstr_rejects_unknown_target() {
        let err = "bogus.foo".parse::<QualifiedTarget>().unwrap_err();
        assert!(err.contains("unknown target"), "got: {}", err);
    }
}
