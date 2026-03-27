mod parser;
mod sessions;
mod search;
mod output;

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use std::io::Write;

use clap::{Parser, Subcommand};
use regex::Regex;
use serde_json::json;

use crate::sessions::{discover_sessions, discover_all_sessions, discover_projects, resolve_session, discover_sessions_with_worktrees};
use crate::search::{search_sessions, SearchOptions};
use crate::output::{format_diff, format_match, format_summary, format_project_header, format_multi_summary, reset_truncation_state, get_did_truncate, format_record};
use crate::parser::Target;

#[derive(Parser)]
#[command(name = "claugrep", about = "Browse, search, and export Claude conversation transcripts")]
struct Cli {
    /// Claude config directory (default: ~/.claude, overrides CLAUDE_CONFIG_DIR env var)
    #[arg(long, global = true)]
    config_dir: Option<PathBuf>,

    /// Filter to a specific account (claudex multi-account support)
    #[arg(long, global = true)]
    account: Option<String>,

    /// Only show sessions modified after the given date (git-compatible: yesterday, '2 days ago', '2026-03-24', Monday, 'last week')
    #[arg(long = "after", alias = "since", global = true)]
    after: Option<String>,

    /// Only show sessions modified before the given date (git-compatible: yesterday, '2 days ago', '2026-03-24', Monday, 'last week')
    #[arg(long = "before", alias = "until", global = true)]
    before: Option<String>,

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

        /// Show raw key/value format for Edit tool matches instead of unified diff
        #[arg(long = "no-diff")]
        no_diff: bool,

        /// Search all projects under ~/.claude/projects/ (ignores --project path)
        #[arg(long = "all-projects")]
        all_projects: bool,

        /// Search only projects whose path matches REGEXP, can be repeated (ignores --project path)
        #[arg(short = 'P', long = "project-regexp", value_name = "REGEXP")]
        project_regexp: Vec<String>,
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

fn all_targets() -> HashSet<Target> {
    [
        Target::User, Target::Assistant, Target::BashCommand, Target::BashOutput,
        Target::ToolUse, Target::ToolResult, Target::SubagentPrompt, Target::CompactSummary,
    ].into_iter().collect()
}

fn parse_targets(s: &str) -> HashSet<Target> {
    s.split(',').filter_map(|t| match t.trim() {
        "user" => Some(Target::User),
        "assistant" => Some(Target::Assistant),
        "bash-command" => Some(Target::BashCommand),
        "bash-output" => Some(Target::BashOutput),
        "tool-use" => Some(Target::ToolUse),
        "tool-result" => Some(Target::ToolResult),
        "subagent-prompt" => Some(Target::SubagentPrompt),
        "compact-summary" => Some(Target::CompactSummary),
        other => { eprintln!("warning: unknown target '{}', ignoring", other); None }
    }).collect()
}

fn parse_since_date(value: &str) -> Result<chrono::DateTime<chrono::Utc>, String> {
    use chrono::{Datelike, Duration, NaiveDate, Utc, Weekday};

    let today = Utc::now().date_naive();
    let val = value.trim().to_lowercase();

    let date_to_dt = |d: NaiveDate| -> chrono::DateTime<Utc> {
        d.and_hms_opt(0, 0, 0).unwrap().and_utc()
    };

    // ISO date: 2026-03-24
    if let Ok(date) = NaiveDate::parse_from_str(&val, "%Y-%m-%d") {
        return Ok(date_to_dt(date));
    }

    // Special keywords
    match val.as_str() {
        "yesterday"  => return Ok(date_to_dt(today - Duration::days(1))),
        "today"      => return Ok(date_to_dt(today)),
        "last week"  => return Ok(date_to_dt(today - Duration::weeks(1))),
        "last month" => return Ok(date_to_dt(today - Duration::days(30))),
        "last year"  => return Ok(date_to_dt(today - Duration::days(365))),
        _ => {}
    }

    // "N unit(s) ago"
    if let Some(rest) = val.strip_suffix(" ago") {
        let parts: Vec<&str> = rest.trim().splitn(2, ' ').collect();
        if parts.len() == 2 {
            if let Ok(n) = parts[0].parse::<i64>() {
                let unit = parts[1].trim().trim_end_matches('s'); // strip plural
                return match unit {
                    "day"    => Ok(date_to_dt(today - Duration::days(n))),
                    "week"   => Ok(date_to_dt(today - Duration::weeks(n))),
                    "month"  => Ok(date_to_dt(today - Duration::days(n * 30))),
                    "year"   => Ok(date_to_dt(today - Duration::days(n * 365))),
                    "hour"   => Ok(Utc::now() - Duration::hours(n)),
                    "minute" => Ok(Utc::now() - Duration::minutes(n)),
                    _ => Err(format!("unknown time unit '{}' in '{}'", unit, value)),
                };
            }
        }
    }

    // Named weekday: most recent occurrence (including today)
    let weekday = match val.as_str() {
        "monday"    => Some(Weekday::Mon),
        "tuesday"   => Some(Weekday::Tue),
        "wednesday" => Some(Weekday::Wed),
        "thursday"  => Some(Weekday::Thu),
        "friday"    => Some(Weekday::Fri),
        "saturday"  => Some(Weekday::Sat),
        "sunday"    => Some(Weekday::Sun),
        _ => None,
    };
    if let Some(wd) = weekday {
        let mut date = today;
        for _ in 0..7 {
            if date.weekday() == wd {
                return Ok(date_to_dt(date));
            }
            date = date - Duration::days(1);
        }
    }

    Err(format!(
        "cannot parse date '{}'; supported formats: 2026-03-24, yesterday, '2 days ago', 'last week', Monday",
        value
    ))
}

fn filter_sessions_since(
    sessions: Vec<sessions::SessionFile>,
    since: Option<chrono::DateTime<chrono::Utc>>,
) -> Vec<sessions::SessionFile> {
    match since {
        None => sessions,
        Some(cutoff) => sessions
            .into_iter()
            .filter(|s| {
                let mtime: chrono::DateTime<chrono::Utc> = s.mtime.into();
                mtime >= cutoff
            })
            .collect(),
    }
}

fn filter_sessions_before(
    sessions: Vec<sessions::SessionFile>,
    before: Option<chrono::DateTime<chrono::Utc>>,
) -> Vec<sessions::SessionFile> {
    match before {
        None => sessions,
        Some(cutoff) => sessions
            .into_iter()
            .filter(|s| {
                let mtime: chrono::DateTime<chrono::Utc> = s.mtime.into();
                mtime < cutoff
            })
            .collect(),
    }
}

fn resolve_project(path: &PathBuf) -> String {
    path.canonicalize()
        .unwrap_or_else(|_| path.clone())
        .to_string_lossy()
        .to_string()
}

/// Compute the list of (account_name, config_dir) pairs to use for session discovery.
fn effective_config_dirs(config_dir: Option<&PathBuf>, account: Option<&str>) -> Vec<(Option<String>, PathBuf)> {
    if let Some(dir) = config_dir {
        return vec![(None, dir.clone())];
    }
    if let Some(acct) = account {
        return vec![(Some(acct.to_string()), sessions::claudex_account_config_dir(acct))];
    }
    let mut dirs = vec![(None, sessions::default_claude_config_dir())];
    for acct in sessions::list_claudex_accounts() {
        dirs.push((Some(acct.clone()), sessions::claudex_account_config_dir(&acct)));
    }
    dirs
}

/// Discover sessions across all config dirs for a given project path, deduplicating by file path.
fn discover_sessions_across_configs(project_path: &str, config_dirs: &[(Option<String>, PathBuf)]) -> Vec<sessions::SessionFile> {
    let mut seen_paths = std::collections::HashSet::new();
    let mut all = vec![];
    for (_, config_dir) in config_dirs {
        let sessions = discover_sessions_with_worktrees(project_path, config_dir);
        for s in sessions {
            if seen_paths.insert(s.file_path.to_string_lossy().to_string()) {
                all.push(s);
            }
        }
    }
    all
}

fn main() {
    let cli = Cli::try_parse().unwrap_or_else(|e| {
        e.print().expect("failed to write error");
        std::process::exit(e.exit_code());
    });

    let config_dirs = effective_config_dirs(cli.config_dir.as_ref(), cli.account.as_deref());

    let since: Option<chrono::DateTime<chrono::Utc>> = match cli.after.as_deref() {
        None => None,
        Some(v) => match parse_since_date(v) {
            Ok(dt) => Some(dt),
            Err(e) => {
                eprintln!("error: {}", e);
                std::process::exit(1);
            }
        },
    };

    let before: Option<chrono::DateTime<chrono::Utc>> = match cli.before.as_deref() {
        None => None,
        Some(v) => match parse_since_date(v) {
            Ok(dt) => Some(dt),
            Err(e) => {
                eprintln!("error: {}", e);
                std::process::exit(1);
            }
        },
    };

    match cli.command {
        Commands::Search {
            pattern, user, assistant, bash_command, bash_output,
            tool_use, tool_result, subagent_prompt, compact_summary,
            project, session, context, before_context, after_context,
            max_results, max_line_width, json, sessions_with_matches, ignore_case, no_diff,
            all_projects, project_regexp,
        } => {
            let mut targets: HashSet<Target> = HashSet::new();
            if user { targets.insert(Target::User); }
            if assistant { targets.insert(Target::Assistant); }
            if bash_command { targets.insert(Target::BashCommand); }
            if bash_output { targets.insert(Target::BashOutput); }
            if tool_use { targets.insert(Target::ToolUse); }
            if tool_result { targets.insert(Target::ToolResult); }
            if subagent_prompt { targets.insert(Target::SubagentPrompt); }
            if compact_summary { targets.insert(Target::CompactSummary); }
            if targets.is_empty() { targets = all_targets(); }

            let flags = if ignore_case { "(?i)" } else { "" };
            let escaped = regex::escape(&pattern);
            let literal_pat = Regex::new(&format!("{}{}", flags, escaped))
                .expect("invalid pattern");
            let patterns = if let Ok(regex_pat) = Regex::new(&format!("{}{}", flags, pattern)) {
                vec![regex_pat]
            } else {
                vec![literal_pat]
            };

            let ctx = context.unwrap_or(0);
            // Context lines within diffs default to 3 (standard unified diff); override via -C/-A/-B
            let diff_ctx = context.or_else(|| {
                let bc = before_context.unwrap_or(0);
                let ac = after_context.unwrap_or(0);
                if bc > 0 || ac > 0 { Some(bc.max(ac)) } else { None }
            }).unwrap_or(3);
            let options = SearchOptions {
                patterns: patterns.clone(),
                targets,
                context_before: before_context.unwrap_or(ctx),
                context_after: after_context.unwrap_or(ctx),
                max_results,
                max_line_width,
                json_output: json,
                sessions_with_matches,
                diff_mode: !no_diff,
            };

            let is_multi_project = all_projects || !project_regexp.is_empty();

            if is_multi_project {
                // Compile project regexps
                let proj_regexps: Vec<Regex> = {
                    let mut result = vec![];
                    for p in &project_regexp {
                        match Regex::new(&format!("{}{}", flags, p)) {
                            Ok(r) => result.push(r),
                            Err(e) => {
                                eprintln!("error: invalid project regexp '{}': {}", p, e);
                                std::process::exit(1);
                            }
                        }
                    }
                    result
                };

                let all_project_infos = discover_projects(&config_dirs);
                let filtered_projects: Vec<_> = all_project_infos.iter()
                    .filter(|p| {
                        if proj_regexps.is_empty() {
                            true
                        } else {
                            proj_regexps.iter().any(|r| r.is_match(&p.decoded_path))
                        }
                    })
                    .collect();

                if filtered_projects.is_empty() {
                    eprintln!("No projects matched the given filters");
                    std::process::exit(1);
                }

                let stdout = std::io::stdout();
                let mut total_matches = 0usize;
                let mut total_sessions_searched = 0usize;
                let mut projects_with_results = 0usize;
                let mut first_project_output = true;
                let mut remaining = max_results;

                reset_truncation_state();


                if json {
                    let mut all_output: Vec<serde_json::Value> = vec![];
                    for proj in &filtered_projects {
                        if remaining == 0 { break; }
                        let pp = &proj.decoded_path;
                        let proj_sessions = filter_sessions_before(
                            filter_sessions_since(
                                discover_sessions_across_configs(pp, &config_dirs),
                                since,
                            ),
                            before,
                        );
                        if proj_sessions.is_empty() { continue; }
                        let sessions = match resolve_session(session.as_deref(), &proj_sessions) {
                            Ok(s) => s,
                            Err(_) => continue,
                        };
                        let proj_options = SearchOptions { max_results: remaining, ..options.clone() };
                        let count = search_sessions(&sessions, &proj_options, |m| {
                            all_output.push(json!({
                                "projectPath": pp,
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
                        remaining = remaining.saturating_sub(count);
                    }
                    println!("{}", serde_json::to_string_pretty(&all_output).unwrap());
                } else if sessions_with_matches {
                    let mut seen = std::collections::HashSet::new();
                    for proj in &filtered_projects {
                        if remaining == 0 { break; }
                        let pp = &proj.decoded_path;
                        let proj_sessions = filter_sessions_before(
                            filter_sessions_since(
                                discover_sessions_across_configs(pp, &config_dirs),
                                since,
                            ),
                            before,
                        );
                        if proj_sessions.is_empty() { continue; }
                        let sessions = match resolve_session(session.as_deref(), &proj_sessions) {
                            Ok(s) => s,
                            Err(_) => continue,
                        };
                        let proj_options = SearchOptions { max_results: remaining, ..options.clone() };
                        let count = search_sessions(&sessions, &proj_options, |m| {
                            let path = sessions.iter()
                                .find(|s| s.session_id == m.session_id)
                                .map(|s| s.file_path.to_string_lossy().to_string())
                                .unwrap_or_else(|| m.session_id.clone());
                            if seen.insert(path.clone()) {
                                let mut out = stdout.lock();
                                writeln!(out, "{}", path).unwrap();
                            }
                        });
                        total_matches += count;
                        remaining = remaining.saturating_sub(count);
                    }
                    if total_matches == 0 { std::process::exit(1); }
                } else {
                    for proj in &filtered_projects {
                        if remaining == 0 { break; }
                        let pp = &proj.decoded_path;
                        let proj_sessions = filter_sessions_before(
                            filter_sessions_since(
                                discover_sessions_across_configs(pp, &config_dirs),
                                since,
                            ),
                            before,
                        );
                        if proj_sessions.is_empty() { continue; }
                        let sessions = match resolve_session(session.as_deref(), &proj_sessions) {
                            Ok(s) => s,
                            Err(_) => continue,
                        };
                        total_sessions_searched += sessions.len();

                        // Buffer per-project results so we only print header if there are matches
                        let mut project_lines: Vec<String> = vec![];
                        let mut first_in_proj = true;
                        let proj_options = SearchOptions { max_results: remaining, ..options.clone() };
                        let count = search_sessions(&sessions, &proj_options, |m| {
                            if !first_in_proj { project_lines.push(String::new()); }
                            first_in_proj = false;
                            let rendered = if !no_diff && m.edit_diff.is_some() {
                                format_diff(&m, m.edit_diff.as_ref().unwrap(), &patterns, max_line_width, diff_ctx)
                            } else {
                                format_match(&m, &patterns, max_line_width)
                            };
                            project_lines.push(rendered);
                        });

                        if count > 0 {
                            let mut out = stdout.lock();
                            if !first_project_output { writeln!(out).unwrap(); }
                            first_project_output = false;
                            writeln!(out, "{}", format_project_header(pp)).unwrap();
                            writeln!(out).unwrap();
                            for line in &project_lines {
                                writeln!(out, "{}", line).unwrap();
                            }
                            out.flush().unwrap();
                            projects_with_results += 1;
                        }
                        total_matches += count;
                        remaining = remaining.saturating_sub(count);
                    }
                    println!("{}", format_multi_summary(total_matches, projects_with_results, filtered_projects.len(), total_sessions_searched));
                    if get_did_truncate() {
                        eprintln!("Hint: Some lines were truncated. Use --max-line-width 0 for full output, or --max-line-width <n> to adjust.");
                    }
                }
            } else {
                // Single-project mode (existing behavior unchanged)
                let project_path = resolve_project(&project);

                let all_sessions = filter_sessions_before(
                    filter_sessions_since(
                        discover_sessions_across_configs(&project_path, &config_dirs),
                        since,
                    ),
                    before,
                );

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
                        let rendered = if !no_diff && m.edit_diff.is_some() {
                            format_diff(&m, m.edit_diff.as_ref().unwrap(), &patterns, max_line_width, diff_ctx)
                        } else {
                            format_match(&m, &patterns, max_line_width)
                        };
                        writeln!(out, "{}", rendered).unwrap();
                        out.flush().unwrap();
                    });
                    println!("{}", format_summary(total, &project_path, sessions.len()));
                    if get_did_truncate() {
                        eprintln!("Hint: Some lines were truncated. Use --max-line-width 0 for full output, or --max-line-width <n> to adjust.");
                    }
                }
            }
        }

        Commands::Last { count, project, targets, max_line_width, json } => {
            let target_set = parse_targets(&targets);

            let all_sessions: Vec<_> = filter_sessions_before(
                filter_sessions_since(
                    if let Some(ref proj) = project {
                        let project_path = resolve_project(proj);
                        discover_sessions_across_configs(&project_path, &config_dirs)
                    } else {
                        discover_all_sessions(&config_dirs)
                    },
                    since,
                ),
                before,
            );

            if all_sessions.is_empty() {
                eprintln!("No session files found");
                std::process::exit(1);
            }

            // Collect all content across all sessions
            let mut all_records: Vec<parser::ExtractedContent> = vec![];
            for session in &all_sessions {
                let contents = parser::extract_content(
                    &session.file_path,
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
            // For Sessions command, use the first config dir
            let config_dir = config_dirs.first().map(|(_, d)| d.as_path())
                .unwrap_or_else(|| Path::new(""));
            let sessions = filter_sessions_before(
                filter_sessions_since(
                    discover_sessions(&project_path, None, config_dir),
                    since,
                ),
                before,
            );

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
            let projects = discover_projects(&config_dirs);

            if projects.is_empty() {
                eprintln!("No projects found under ~/.claude/projects/");
                std::process::exit(1);
            }

            let has_multiple_accounts = config_dirs.iter().any(|(acct, _)| acct.is_some());

            if json {
                let output: Vec<_> = projects.iter().map(|p| {
                    let mtime = p.latest_mtime
                        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                        .map(|d| d.as_secs());
                    json!({
                        "path": p.decoded_path,
                        "encodedPath": p.encoded_path,
                        "verified": p.verified,
                        "sessionCount": p.session_count,
                        "latestMtime": mtime,
                        "account": p.account,
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
                    let unverified = if p.verified { "" } else { " [unverified]" };
                    let account_str = if has_multiple_accounts {
                        match &p.account {
                            Some(a) => format!(" [{}]", a),
                            None => " [default]".to_string(),
                        }
                    } else {
                        String::new()
                    };
                    println!("{} ({} session{}) {}{}{}",
                        p.decoded_path,
                        p.session_count,
                        if p.session_count == 1 { "" } else { "s" },
                        ts_str,
                        unverified,
                        account_str);
                }
                eprintln!("{} project{}", projects.len(), if projects.len() == 1 { "" } else { "s" });
            }
        }

        Commands::Dump { session, project, targets } => {
            let project_path = resolve_project(&project);
            let target_set = parse_targets(&targets);

            let all_sessions = filter_sessions_before(
                filter_sessions_since(
                    discover_sessions_across_configs(&project_path, &config_dirs),
                    since,
                ),
                before,
            );
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
                let contents = parser::extract_content(
                    &s.file_path,
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
