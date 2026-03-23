mod parser;
mod sessions;
mod search;
mod output;

use std::collections::HashSet;
use std::path::PathBuf;

use std::io::Write;

use clap::{Parser, Subcommand};
use regex::Regex;
use serde_json::json;

use crate::sessions::{discover_sessions, discover_all_sessions, discover_projects, resolve_session, get_worktree_paths};
use crate::search::{search_sessions, SearchOptions};
use crate::output::{format_match, format_summary, reset_truncation_state, get_did_truncate, format_record};

#[derive(Parser)]
#[command(name = "claugrep", about = "Browse, search, and export Claude conversation transcripts")]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Search Claude Code conversation transcripts
    Search {
        /// Pattern to search (literal string and/or regex)
        pattern: String,

        /// Search user messages
        #[arg(short = 'u', long)]
        user: bool,

        /// Search assistant responses
        #[arg(short = 'a', long)]
        assistant: bool,

        /// Search bash commands
        #[arg(short = 'c', long = "bash-command")]
        bash_command: bool,

        /// Search bash output
        #[arg(short = 'o', long = "bash-output")]
        bash_output: bool,

        /// Search tool use inputs
        #[arg(short = 't', long = "tool-use")]
        tool_use: bool,

        /// Search tool results
        #[arg(short = 'r', long = "tool-result")]
        tool_result: bool,

        /// Search subagent prompts
        #[arg(short = 's', long = "subagent-prompt")]
        subagent_prompt: bool,

        /// Search compact/continuation summaries
        #[arg(long = "compact-summary")]
        compact_summary: bool,

        /// Project path (default: current directory)
        #[arg(long, default_value = ".")]
        project: PathBuf,

        /// Specific session (UUID prefix, offset like -1, or "all")
        #[arg(long)]
        session: Option<String>,

        /// Context lines around matches
        #[arg(short = 'C', long)]
        context: Option<usize>,

        /// Context lines before matches
        #[arg(short = 'B', long = "before-context")]
        before_context: Option<usize>,

        /// Context lines after matches
        #[arg(short = 'A', long = "after-context")]
        after_context: Option<usize>,

        /// Max results
        #[arg(long, default_value = "50")]
        max_results: usize,

        /// Max output line width (0 = unlimited)
        #[arg(long, default_value = "200")]
        max_line_width: usize,

        /// JSON output
        #[arg(long)]
        json: bool,

        /// Only print session IDs with matches
        #[arg(short = 'l', long = "sessions-with-matches")]
        sessions_with_matches: bool,

        /// Case-insensitive search
        #[arg(short = 'i', long = "ignore-case")]
        ignore_case: bool,
    },

    /// List sessions for a project
    Sessions {
        /// Project path (default: current directory)
        #[arg(long, default_value = ".")]
        project: PathBuf,

        /// JSON output
        #[arg(long)]
        json: bool,
    },

    /// Show the last N records across all sessions, sorted by time
    Last {
        /// Number of records to show
        #[arg(short = 'n', long = "last", default_value = "20")]
        count: usize,

        /// Project path (default: all projects)
        #[arg(long)]
        project: Option<PathBuf>,

        /// Content types to include (comma-separated: user,assistant,bash-command,bash-output,tool-use,tool-result,subagent-prompt,compact-summary)
        #[arg(long, default_value = "user,assistant")]
        targets: String,

        /// Max output line width (0 = unlimited)
        #[arg(long, default_value = "200")]
        max_line_width: usize,

        /// JSON output
        #[arg(long)]
        json: bool,
    },

    /// List all known projects under ~/.claude/projects/
    Projects {
        /// JSON output
        #[arg(long)]
        json: bool,
    },

    /// Dump a session's content as plain text
    Dump {
        /// Session ID prefix, offset (e.g. -1 for previous, 0 for latest), or "all"
        #[arg(allow_hyphen_values = true)]
        session: String,

        /// Project path (default: current directory)
        #[arg(long, default_value = ".")]
        project: PathBuf,

        /// Content types to include (comma-separated: user,assistant,bash-command,bash-output,tool-use,tool-result,subagent-prompt,compact-summary)
        #[arg(long, default_value = "user,assistant")]
        targets: String,
    },
}

fn all_targets() -> HashSet<String> {
    [
        "user", "assistant", "bash-command", "bash-output",
        "tool-use", "tool-result", "subagent-prompt", "compact-summary",
    ].iter().map(|s| s.to_string()).collect()
}

fn resolve_project(path: &PathBuf) -> String {
    path.canonicalize()
        .unwrap_or_else(|_| path.clone())
        .to_string_lossy()
        .to_string()
}

fn main() {
    let cli = Cli::parse();

    match cli.command {
        Commands::Search {
            pattern, user, assistant, bash_command, bash_output,
            tool_use, tool_result, subagent_prompt, compact_summary,
            project, session, context, before_context, after_context,
            max_results, max_line_width, json, sessions_with_matches, ignore_case,
        } => {
            let project_path = resolve_project(&project);

            let mut targets: HashSet<String> = HashSet::new();
            if user { targets.insert("user".into()); }
            if assistant { targets.insert("assistant".into()); }
            if bash_command { targets.insert("bash-command".into()); }
            if bash_output { targets.insert("bash-output".into()); }
            if tool_use { targets.insert("tool-use".into()); }
            if tool_result { targets.insert("tool-result".into()); }
            if subagent_prompt { targets.insert("subagent-prompt".into()); }
            if compact_summary { targets.insert("compact-summary".into()); }
            if targets.is_empty() { targets = all_targets(); }

            let flags = if ignore_case { "(?i)" } else { "" };
            let escaped = regex::escape(&pattern);
            let literal_pat = Regex::new(&format!("{}{}", flags, escaped))
                .expect("invalid pattern");
            let mut patterns = vec![literal_pat.clone()];
            if let Ok(regex_pat) = Regex::new(&format!("{}{}", flags, pattern)) {
                if regex_pat.as_str() != literal_pat.as_str() {
                    patterns.push(regex_pat);
                }
            }

            let ctx = context.unwrap_or(0);
            let options = SearchOptions {
                patterns: patterns.clone(),
                targets,
                context_before: before_context.unwrap_or(ctx),
                context_after: after_context.unwrap_or(ctx),
                max_results,
                max_line_width,
                json_output: json,
                sessions_with_matches,
            };

            // Collect sessions from all git worktrees, deduplicating by file path
            let worktree_paths = get_worktree_paths(&project_path);
            let mut unique_paths: Vec<String> = worktree_paths;
            if !unique_paths.contains(&project_path) {
                unique_paths.push(project_path.clone());
            }
            let mut seen_paths = std::collections::HashSet::new();
            let all_sessions: Vec<_> = unique_paths.iter()
                .flat_map(|p| discover_sessions(p, None))
                .filter(|s| seen_paths.insert(s.file_path.to_string_lossy().to_string()))
                .collect();

            if all_sessions.is_empty() {
                eprintln!("No session files found for project {}", project_path);
                std::process::exit(1);
            }

            let sessions = match resolve_session(session.as_deref(), &all_sessions) {
                Ok(s) => s,
                Err(e) => {
                    eprintln!("{}", e);
                    std::process::exit(1);
                }
            };
            let stdout = std::io::stdout();

            if sessions_with_matches {
                let mut seen = std::collections::HashSet::new();
                let total = search_sessions(&sessions, &options, |m| {
                    let path = sessions.iter()
                        .find(|s| s.session_id == m.session_id)
                        .map(|s| s.file_path.to_string_lossy().to_string())
                        .unwrap_or_else(|| m.session_id.clone());
                    if seen.insert(path.clone()) {
                        let mut out = stdout.lock();
                        writeln!(out, "{}", path).unwrap();
                    }
                });
                if total == 0 { std::process::exit(1); }
            } else if json {
                // JSON must be a single valid array, so collect then print.
                let mut output: Vec<serde_json::Value> = vec![];
                search_sessions(&sessions, &options, |m| {
                    output.push(json!({
                        "matchNumber": m.match_number,
                        "sessionId": m.session_id,
                        "timestamp": m.timestamp,
                        "target": m.target.as_str(),
                        "toolName": m.tool_name,
                        "matchedLines": m.matched_lines.iter().map(|ml| json!({
                            "lineNumber": ml.line_number,
                            "line": ml.line,
                            "isMatch": ml.is_match,
                        })).collect::<Vec<_>>(),
                    }));
                });
                println!("{}", serde_json::to_string_pretty(&output).unwrap());
            } else {
                reset_truncation_state();
                let mut first = true;
                let total = search_sessions(&sessions, &options, |m| {
                    let mut out = stdout.lock();
                    if !first { writeln!(out).unwrap(); }
                    first = false;
                    writeln!(out, "{}", format_match(&m, &patterns, max_line_width)).unwrap();
                    out.flush().unwrap();
                });
                println!("{}", format_summary(total, &project_path, sessions.len()));
                if get_did_truncate() {
                    eprintln!("Hint: Some lines were truncated. Use --max-line-width 0 for full output, or --max-line-width <n> to adjust.");
                }
            }
        }

        Commands::Last { count, project, targets, max_line_width, json } => {
            let target_set: HashSet<String> = targets.split(',')
                .map(|s| s.trim().to_string())
                .collect();

            let all_sessions: Vec<_> = if let Some(ref proj) = project {
                let project_path = resolve_project(proj);
                let worktree_paths = get_worktree_paths(&project_path);
                let mut unique_paths: Vec<String> = worktree_paths;
                if !unique_paths.contains(&project_path) {
                    unique_paths.push(project_path.clone());
                }
                let mut seen_paths = std::collections::HashSet::new();
                unique_paths.iter()
                    .flat_map(|p| discover_sessions(p, None))
                    .filter(|s| seen_paths.insert(s.file_path.to_string_lossy().to_string()))
                    .collect()
            } else {
                discover_all_sessions()
            };

            if all_sessions.is_empty() {
                eprintln!("No session files found");
                std::process::exit(1);
            }

            // Collect all content across all sessions
            let mut all_records: Vec<parser::ExtractedContent> = vec![];
            for session in &all_sessions {
                let tool_use_map = parser::build_tool_use_map(&session.file_path);
                let contents = parser::extract_content(
                    &session.file_path,
                    &tool_use_map,
                    &target_set,
                    &session.session_id,
                    session.is_subagent,
                );
                all_records.extend(contents);
            }

            // Sort by timestamp (ISO 8601 lexicographic order works)
            all_records.sort_by(|a, b| a.timestamp.cmp(&b.timestamp));

            // Take the last N
            let start = all_records.len().saturating_sub(count);
            let records = &all_records[start..];

            if json {
                let output: Vec<_> = records.iter().map(|r| serde_json::json!({
                    "sessionId": r.session_id,
                    "timestamp": r.timestamp,
                    "target": r.target.as_str(),
                    "toolName": r.tool_name,
                    "text": r.text,
                })).collect();
                println!("{}", serde_json::to_string_pretty(&output).unwrap());
            } else {
                for r in records {
                    println!("{}", format_record(r, max_line_width));
                }
                eprintln!("Showing {} of {} record{} across {} session{}",
                    records.len(), all_records.len(),
                    if all_records.len() == 1 { "" } else { "s" },
                    all_sessions.len(),
                    if all_sessions.len() == 1 { "" } else { "s" });
            }
        }

        Commands::Sessions { project, json } => {
            let project_path = resolve_project(&project);
            let sessions = discover_sessions(&project_path, None);

            if sessions.is_empty() {
                eprintln!("No sessions found for project {}", project_path);
                std::process::exit(1);
            }

            if json {
                let output: Vec<_> = sessions.iter().map(|s| {
                    let mtime = s.mtime.duration_since(std::time::UNIX_EPOCH)
                        .map(|d| d.as_secs()).unwrap_or(0);
                    json!({
                        "sessionId": s.session_id,
                        "filePath": s.file_path.to_string_lossy(),
                        "mtime": mtime,
                        "isSubagent": s.is_subagent,
                    })
                }).collect();
                println!("{}", serde_json::to_string_pretty(&output).unwrap());
            } else {
                let mut count = 0;
                for s in &sessions {
                    if s.is_subagent { continue; }
                    let mtime: chrono::DateTime<chrono::Utc> = s.mtime.into();
                    println!("{} {}", mtime.format("%Y-%m-%d %H:%M:%S"), s.session_id);
                    count += 1;
                }
                eprintln!("{} session{}", count, if count == 1 { "" } else { "s" });
            }
        }

        Commands::Projects { json } => {
            let projects = discover_projects();

            if projects.is_empty() {
                eprintln!("No projects found under ~/.claude/projects/");
                std::process::exit(1);
            }

            if json {
                let output: Vec<_> = projects.iter().map(|p| {
                    let mtime = p.latest_mtime
                        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                        .map(|d| d.as_secs());
                    json!({
                        "path": p.decoded_path,
                        "encodedPath": p.encoded_path,
                        "sessionCount": p.session_count,
                        "latestMtime": mtime,
                    })
                }).collect();
                println!("{}", serde_json::to_string_pretty(&output).unwrap());
            } else {
                for p in &projects {
                    let ts_str = p.latest_mtime
                        .map(|t| {
                            let dt: chrono::DateTime<chrono::Utc> = t.into();
                            dt.format("%Y-%m-%d %H:%M:%S").to_string()
                        })
                        .unwrap_or_else(|| "no sessions".to_string());
                    println!("{} ({} session{}) {}",
                        p.decoded_path,
                        p.session_count,
                        if p.session_count == 1 { "" } else { "s" },
                        ts_str);
                }
                eprintln!("{} project{}", projects.len(), if projects.len() == 1 { "" } else { "s" });
            }
        }

        Commands::Dump { session, project, targets } => {
            let project_path = resolve_project(&project);
            let target_set: HashSet<String> = targets.split(',')
                .map(|s| s.trim().to_string())
                .collect();

            let all_sessions = discover_sessions(&project_path, None);
            let sessions = match resolve_session(Some(&session), &all_sessions) {
                Ok(s) => s,
                Err(e) => {
                    eprintln!("{}", e);
                    std::process::exit(1);
                }
            };

            if sessions.is_empty() {
                eprintln!("No sessions found matching '{}'", session);
                std::process::exit(1);
            }

            for s in &sessions {
                let tool_use_map = parser::build_tool_use_map(&s.file_path);
                let contents = parser::extract_content(
                    &s.file_path,
                    &tool_use_map,
                    &target_set,
                    &s.session_id,
                    s.is_subagent,
                );
                for content in contents {
                    let label = match &content.tool_name {
                        Some(t) => format!("[{}:{}]", content.target.as_str(), t),
                        None => format!("[{}]", content.target.as_str()),
                    };
                    println!("{} {}", label, content.text);
                }
            }
        }
    }
}
