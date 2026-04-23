mod parser;
mod sessions;
mod search;
mod output;
mod memory;

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use std::io::Write;

use clap::{Parser, Subcommand, ValueEnum};
use regex::Regex;
use serde_json::json;

use crate::sessions::{discover_sessions, discover_all_sessions, discover_projects, resolve_session, discover_sessions_with_worktrees};
use crate::search::{search_sessions, SearchOptions, find_matches};
use crate::output::{format_diff, format_edit_diff, format_match, format_summary, format_project_header, format_multi_summary, reset_truncation_state, get_did_truncate, format_record};
use crate::parser::Target;
use crate::memory::{discover_memory_files, MemoryFile};

#[derive(Clone, Copy, Debug, ValueEnum)]
enum ColorWhen {
    /// Colorize output only when writing to a terminal
    Auto,
    /// Always colorize output
    Always,
    /// Never colorize output
    Never,
}

#[derive(Parser)]
#[command(name = "claugrep", about = "Browse, search, and export Claude conversation transcripts")]
struct Cli {
    /// Claude config directory (default: ~/.claude, overrides CLAUDE_CONFIG_DIR env var)
    #[arg(long, global = true)]
    config_dir: Option<PathBuf>,

    /// Filter to a specific account (claudex multi-account support)
    #[arg(long, global = true)]
    account: Option<String>,

    /// When to use colors: auto, always, never (also respects NO_COLOR env var)
    #[arg(long, global = true, default_value = "auto", value_name = "WHEN")]
    color: ColorWhen,

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
    #[command(alias = "s")]
    Search {
        /// Pattern to search (literal string and/or regex)
        pattern: String,

        /// Content types to include (comma-separated: user,assistant,thinking,bash-command,bash-output,tool-use,tool-result,subagent-prompt,compact-summary,system,file-history-snapshot,queue-operation,last-prompt,agent-name,custom-title,permission-mode,attachment,progress; or "default" for standard types, "all" for everything including internals)
        #[arg(short = 't', long, default_value = "default")]
        targets: String,

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

        /// Record-level context: N records before the match and N records after.
        #[arg(long = "around-records", value_name = "N")]
        around_records: Option<usize>,

        /// Record-level context: N records immediately before the match.
        #[arg(long = "before-records", value_name = "N")]
        before_records: Option<usize>,

        /// Record-level context: N records immediately after the match.
        #[arg(long = "after-records", value_name = "N")]
        after_records: Option<usize>,

        /// Record-level context spec: signed offsets and inclusive ranges, comma-separated
        /// (e.g. "5", "-3..-1", "-3..3", "-2,2,5"). Offset 0 is ignored (the match is always shown).
        #[arg(long = "records", value_name = "SPEC", allow_hyphen_values = true)]
        records: Option<String>,

        /// Restrict record-level context to these types; offsets advance only over matching
        /// types and non-matching records are hidden. Accepts the same tokens as -t.
        #[arg(long = "records-type", value_name = "TYPES")]
        records_type: Option<String>,

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

        /// Treat pattern as a fixed string (no regex interpretation)
        #[arg(short = 'F', long = "fixed-strings")]
        fixed_strings: bool,

        /// Treat pattern as an extended regular expression (no literal fallback)
        #[arg(short = 'E', long = "extended-regexp")]
        extended_regexp: bool,

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

        /// Content types to include (comma-separated; or "default" for standard types, "all" for everything including internals)
        #[arg(short = 't', long, default_value = "default")]
        targets: String,

        /// Max output line width (0 = unlimited)
        #[arg(long, default_value = "200")]
        max_line_width: usize,

        /// Show raw key/value format for Edit tool matches instead of unified diff
        #[arg(long = "no-diff")]
        no_diff: bool,

        /// JSON output
        #[arg(long)]
        json: bool,
    },

    /// List all known projects under ~/.claude/projects/
    Projects {
        /// JSON output
        #[arg(long)]
        json: bool,

        /// List sessions within each project
        #[arg(short = 's', long)]
        sessions: bool,
    },

    /// Dump a session's content as plain text
    Dump {
        /// Session ID prefix, offset (e.g. -1 for previous, 0 for latest), or "all" (default: 0)
        #[arg(allow_hyphen_values = true, default_value = "0")]
        session: String,

        /// Project path (default: current directory)
        #[arg(long, default_value = ".")]
        project: PathBuf,

        /// Content types to include (comma-separated; or "default" for standard types, "all" for everything including internals)
        #[arg(short = 't', long, default_value = "default")]
        targets: String,

        /// Show raw key/value format for Edit tool matches instead of unified diff
        #[arg(long = "no-diff")]
        no_diff: bool,

        /// JSON output (raw JSONL records)
        #[arg(long)]
        json: bool,

        /// Include subagent transcripts
        #[arg(long)]
        subagents: bool,
    },

    /// Show the last N records of a session (like tail)
    Tail {
        /// Number of records to show
        #[arg(short = 'n', long = "lines", default_value = "10")]
        count: usize,

        /// Follow the session file for new records (like tail -f)
        #[arg(short = 'f', long)]
        follow: bool,

        /// Session ID prefix, offset (e.g. -1 for previous, 0 for latest), or "all" (default: 0)
        #[arg(allow_hyphen_values = true, default_value = "0")]
        session: String,

        /// Project path (default: current directory)
        #[arg(long, default_value = ".")]
        project: PathBuf,

        /// Content types to include (comma-separated; or "default" for standard types, "all" for everything including internals)
        #[arg(short = 't', long, default_value = "default")]
        targets: String,

        /// Show raw key/value format for Edit tool matches instead of unified diff
        #[arg(long = "no-diff")]
        no_diff: bool,

        /// JSON output (raw JSONL records)
        #[arg(long)]
        json: bool,

        /// Include subagent transcripts
        #[arg(long)]
        subagents: bool,
    },

    /// Inspect the CLAUDE.md and auto-memory markdown files that apply to a directory
    Memory {
        #[command(subcommand)]
        subcommand: MemoryCommands,
    },
}

#[derive(Subcommand)]
enum MemoryCommands {
    /// Print every markdown memory file that applies to the project
    Dump {
        /// Project path (default: current directory)
        #[arg(long, default_value = ".")]
        project: PathBuf,

        /// Exclude on-demand CLAUDE.md files in subdirectories
        #[arg(long = "no-subdirs")]
        no_subdirs: bool,

        /// JSON output (one object per file with content inlined)
        #[arg(long)]
        json: bool,

        /// Only print the list of discovered file paths
        #[arg(short = 'l', long = "files-only")]
        files_only: bool,
    },

    /// Search markdown memory files that apply to the project
    Search {
        /// Pattern to search (literal string and/or regex)
        pattern: String,

        /// Project path (default: current directory)
        #[arg(long, default_value = ".")]
        project: PathBuf,

        /// Exclude on-demand CLAUDE.md files in subdirectories
        #[arg(long = "no-subdirs")]
        no_subdirs: bool,

        /// Context lines around matches
        #[arg(short = 'C', long)]
        context: Option<usize>,

        /// Context lines before matches
        #[arg(short = 'B', long = "before-context")]
        before_context: Option<usize>,

        /// Context lines after matches
        #[arg(short = 'A', long = "after-context")]
        after_context: Option<usize>,

        /// Max output line width (0 = unlimited)
        #[arg(long, default_value = "200")]
        max_line_width: usize,

        /// Max results
        #[arg(long, default_value = "50")]
        max_results: usize,

        /// JSON output
        #[arg(long)]
        json: bool,

        /// Only print file paths with matches
        #[arg(short = 'l', long = "files-with-matches")]
        files_with_matches: bool,

        /// Case-insensitive search
        #[arg(short = 'i', long = "ignore-case")]
        ignore_case: bool,

        /// Treat pattern as a fixed string (no regex interpretation)
        #[arg(short = 'F', long = "fixed-strings")]
        fixed_strings: bool,

        /// Treat pattern as an extended regular expression (no literal fallback)
        #[arg(short = 'E', long = "extended-regexp")]
        extended_regexp: bool,
    },
}

fn default_targets() -> HashSet<Target> {
    [
        Target::User, Target::Assistant, Target::Thinking, Target::BashCommand, Target::BashOutput,
        Target::ToolUse, Target::ToolResult, Target::SubagentPrompt, Target::CompactSummary,
        Target::QueueOperation,
    ].into_iter().collect()
}

fn all_targets() -> HashSet<Target> {
    let mut t = default_targets();
    t.insert(Target::System);
    t.insert(Target::FileHistorySnapshot);
    t.insert(Target::LastPrompt);
    t.insert(Target::AgentName);
    t.insert(Target::CustomTitle);
    t.insert(Target::PermissionMode);
    t.insert(Target::Attachment);
    t.insert(Target::Progress);
    t.insert(Target::PullRequest);
    t
}

/// Parse a record-context SPEC like "5", "-3..3", "-5..-1,1..5" into sorted, deduped offsets.
/// Offset 0 is silently dropped (the match itself is always shown). Returns an error for
/// unparseable tokens or reversed ranges (M..N with M > N).
fn parse_records_spec(spec: &str) -> Result<Vec<i32>, String> {
    let mut offsets: std::collections::BTreeSet<i32> = Default::default();
    for raw in spec.split(',') {
        let part = raw.trim();
        if part.is_empty() {
            continue;
        }

        if let Some(idx) = part.find("..") {
            let lhs = part[..idx].trim();
            let rhs = part[idx + 2..].trim();
            if lhs.is_empty() || rhs.is_empty() {
                return Err(format!(
                    "open-ended range '{}' not supported; both endpoints required",
                    part
                ));
            }
            let start: i32 = lhs.parse()
                .map_err(|_| format!("invalid offset '{}' in range '{}'", lhs, part))?;
            let end: i32 = rhs.parse()
                .map_err(|_| format!("invalid offset '{}' in range '{}'", rhs, part))?;
            if end < start {
                return Err(format!(
                    "range '{}' is reversed ({} > {}); start must be <= end", part, start, end
                ));
            }
            for n in start..=end {
                if n != 0 {
                    offsets.insert(n);
                }
            }
        } else {
            let n: i32 = part.parse()
                .map_err(|_| format!("invalid offset '{}'", part))?;
            if n != 0 {
                offsets.insert(n);
            }
        }
    }
    Ok(offsets.into_iter().collect())
}

/// Combine shorthand record-context flags with the explicit --records SPEC.
/// Returns the merged sorted/deduped offsets, or an error if SPEC is malformed.
fn merge_record_context(
    around: Option<usize>,
    before: Option<usize>,
    after: Option<usize>,
    spec: Option<&str>,
) -> Result<Vec<i32>, String> {
    let mut offsets: std::collections::BTreeSet<i32> = Default::default();
    if let Some(n) = around {
        let n = n as i32;
        for i in 1..=n {
            offsets.insert(-i);
            offsets.insert(i);
        }
    }
    if let Some(n) = before {
        let n = n as i32;
        for i in 1..=n {
            offsets.insert(-i);
        }
    }
    if let Some(n) = after {
        let n = n as i32;
        for i in 1..=n {
            offsets.insert(i);
        }
    }
    if let Some(s) = spec {
        for off in parse_records_spec(s)? {
            offsets.insert(off);
        }
    }
    Ok(offsets.into_iter().collect())
}

fn parse_targets(s: &str) -> HashSet<Target> {
    let mut out: HashSet<Target> = HashSet::new();
    for tok in s.split(',') {
        match tok.trim() {
            "" => continue,
            "default" => out.extend(default_targets()),
            "all" => out.extend(all_targets()),
            "user" => { out.insert(Target::User); }
            "assistant" => { out.insert(Target::Assistant); }
            "thinking" => { out.insert(Target::Thinking); }
            "bash-command" => { out.insert(Target::BashCommand); }
            "bash-output" => { out.insert(Target::BashOutput); }
            "tool-use" => { out.insert(Target::ToolUse); }
            "tool-result" => { out.insert(Target::ToolResult); }
            "subagent-prompt" => { out.insert(Target::SubagentPrompt); }
            "compact-summary" => { out.insert(Target::CompactSummary); }
            "system" => { out.insert(Target::System); }
            "file-history-snapshot" => { out.insert(Target::FileHistorySnapshot); }
            "queue-operation" => { out.insert(Target::QueueOperation); }
            "last-prompt" => { out.insert(Target::LastPrompt); }
            "agent-name" => { out.insert(Target::AgentName); }
            "custom-title" => { out.insert(Target::CustomTitle); }
            "permission-mode" => { out.insert(Target::PermissionMode); }
            "attachment" => { out.insert(Target::Attachment); }
            "progress" => { out.insert(Target::Progress); }
            "pull-request" => { out.insert(Target::PullRequest); }
            other => eprintln!("warning: unknown target '{}', ignoring", other),
        }
    }
    out
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

/// Emit a SearchMatch as JSON to stdout.
/// With `wrap_context = false` this preserves the historical stream format
/// (one raw entry per line). With `wrap_context = true` each match is wrapped
/// in `{"match": raw, "context": [{"offset": n, "entry": raw}, ...]}` so the
/// neighboring records the user asked for can be grouped with their match.
fn emit_json_match(m: &crate::search::SearchMatch, wrap_context: bool) {
    if wrap_context {
        let ctx: Vec<serde_json::Value> = m.context_records.iter().map(|c| json!({
            "offset": c.offset,
            "entry": c.raw_entry,
        })).collect();
        let obj = json!({
            "match": m.raw_entry,
            "context": ctx,
        });
        println!("{}", obj);
    } else if let Some(ref raw) = m.raw_entry {
        println!("{}", raw);
    }
}

fn print_dump_record(content: &parser::ExtractedContent, json: bool, no_diff: bool) {
    if json {
        if let Some(ref raw) = content.raw_entry {
            println!("{}", raw);
        }
        return;
    }
    let label = match &content.tool_name {
        Some(t) => format!("[{}:{}]", content.target.as_str(), t),
        None => format!("[{}]", content.target.as_str()),
    };
    let label = console::style(label).dim();
    if !no_diff {
        if let Some(ref diff) = content.edit_diff {
            println!("{}\n{}", label, format_edit_diff(diff));
            return;
        }
    }
    let sep = if content.text.contains('\n') { "\n" } else { " " };
    println!("{}{}{}", label, sep, content.text);
}

fn main() {
    // Reset SIGPIPE to default so that writing to a closed pipe (e.g. `claugrep | head`)
    // causes the kernel to kill the process cleanly instead of Rust's panic handler firing.
    #[cfg(unix)]
    unsafe {
        libc::signal(libc::SIGPIPE, libc::SIG_DFL);
    }

    let cli = Cli::try_parse().unwrap_or_else(|e| {
        e.print().expect("failed to write error");
        std::process::exit(e.exit_code());
    });

    // Configure color output (console crate already respects NO_COLOR, CLICOLOR, CLICOLOR_FORCE)
    match cli.color {
        ColorWhen::Always => {
            console::set_colors_enabled(true);
            console::set_colors_enabled_stderr(true);
        }
        ColorWhen::Never => {
            console::set_colors_enabled(false);
            console::set_colors_enabled_stderr(false);
        }
        ColorWhen::Auto => {}
    }

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
            pattern, targets: targets_str,
            project, session, context, before_context, after_context,
            around_records, before_records, after_records, records, records_type,
            max_results, max_line_width, json, sessions_with_matches, ignore_case, no_diff,
            fixed_strings, extended_regexp,
            all_projects, project_regexp,
        } => {
            let targets = parse_targets(&targets_str);

            let context_offsets = match merge_record_context(
                around_records, before_records, after_records, records.as_deref(),
            ) {
                Ok(v) => v,
                Err(e) => {
                    eprintln!("error: {}", e);
                    std::process::exit(2);
                }
            };
            let context_type_filter: Option<HashSet<Target>> = records_type
                .as_deref()
                .map(parse_targets);

            // When record-context is requested, extract all known types so offsets
            // can walk over any record; otherwise just the -t targets are enough.
            let extract_targets: HashSet<Target> = if context_offsets.is_empty() {
                targets.clone()
            } else {
                all_targets()
            };

            let flags = if ignore_case { "(?i)" } else { "" };
            let escaped = regex::escape(&pattern);
            let patterns = if fixed_strings {
                vec![Regex::new(&format!("{}{}", flags, escaped)).expect("invalid pattern")]
            } else if extended_regexp {
                match Regex::new(&format!("{}{}", flags, pattern)) {
                    Ok(r) => vec![r],
                    Err(e) => {
                        eprintln!("error: invalid regex '{}': {}", pattern, e);
                        std::process::exit(2);
                    }
                }
            } else {
                // Default: try as regex, fall back to literal
                let literal_pat = Regex::new(&format!("{}{}", flags, escaped))
                    .expect("invalid pattern");
                if let Ok(regex_pat) = Regex::new(&format!("{}{}", flags, pattern)) {
                    vec![regex_pat]
                } else {
                    vec![literal_pat]
                }
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
                extract_targets,
                context_before: before_context.unwrap_or(ctx),
                context_after: after_context.unwrap_or(ctx),
                max_results,
                max_line_width,
                json_output: json,
                sessions_with_matches,
                diff_mode: !no_diff,
                context_offsets,
                context_type_filter,
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
                        let wrap_context = !proj_options.context_offsets.is_empty();
                        let count = search_sessions(&sessions, &proj_options, |m| {
                            emit_json_match(&m, wrap_context);
                        });
                        remaining = remaining.saturating_sub(count);
                    }
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
                    if total_matches == max_results {
                        eprintln!("Hint: Result limit reached. Use --max-results to increase the limit.");
                    }
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
                    let wrap_context = !options.context_offsets.is_empty();
                    search_sessions(&sessions, &options, |m| {
                        emit_json_match(&m, wrap_context);
                    });
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
                    if total == max_results {
                        eprintln!("Hint: Result limit reached. Use --max-results to increase the limit.");
                    }
                    if get_did_truncate() {
                        eprintln!("Hint: Some lines were truncated. Use --max-line-width 0 for full output, or --max-line-width <n> to adjust.");
                    }
                }
            }
        }

        Commands::Last { count, project, targets, max_line_width, no_diff, json } => {
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
                let contents = parser::extract_content_opts(
                    &session.file_path,
                    &target_set,
                    &session.session_id,
                    session.is_subagent,
                    json,
                );
                all_records.extend(contents);
            }

            // Sort by timestamp (ISO 8601 lexicographic order works)
            all_records.sort_by(|a, b| a.timestamp.cmp(&b.timestamp));

            // Take the last N
            let start = all_records.len().saturating_sub(count);
            let records = &all_records[start..];

            if json {
                for r in records {
                    if let Some(ref raw) = r.raw_entry {
                        println!("{}", raw);
                    }
                }
            } else {
                for r in records {
                    if !no_diff {
                        if let Some(ref diff) = r.edit_diff {
                            println!("{}\n{}", format_record(r, max_line_width), format_edit_diff(diff));
                            continue;
                        }
                    }
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
            let sessions = filter_sessions_before(
                filter_sessions_since(
                    discover_sessions_across_configs(&project_path, &config_dirs),
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

        Commands::Projects { json, sessions: list_sessions } => {
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
                    let mut entry = json!({
                        "path": p.decoded_path,
                        "encodedPath": p.encoded_path,
                        "verified": p.verified,
                        "sessionCount": p.session_count,
                        "latestMtime": mtime,
                        "account": p.account,
                    });
                    if list_sessions {
                        let config_dir = config_dirs.iter()
                            .find(|(acct, _)| acct == &p.account)
                            .map(|(_, d)| d.as_path())
                            .unwrap_or_else(|| config_dirs.first().map(|(_, d)| d.as_path()).unwrap());
                        let sess = filter_sessions_before(
                            filter_sessions_since(
                                discover_sessions(&p.decoded_path, None, config_dir),
                                since,
                            ),
                            before,
                        );
                        let sess_json: Vec<_> = sess.iter().map(|s| {
                            let smtime = s.mtime.duration_since(std::time::UNIX_EPOCH)
                                .map(|d| d.as_secs()).unwrap_or(0);
                            json!({
                                "sessionId": s.session_id,
                                "filePath": s.file_path.to_string_lossy(),
                                "mtime": smtime,
                                "isSubagent": s.is_subagent,
                            })
                        }).collect();
                        entry["sessions"] = json!(sess_json);
                    }
                    entry
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
                    let account_str = if has_multiple_accounts {
                        match &p.account {
                            Some(a) => format!(" [{}]", a),
                            None => " [default]".to_string(),
                        }
                    } else {
                        String::new()
                    };
                    println!("{} ({} session{}) {}{}",
                        p.decoded_path,
                        p.session_count,
                        if p.session_count == 1 { "" } else { "s" },
                        ts_str,
                        account_str);
                    if list_sessions {
                        let config_dir = config_dirs.iter()
                            .find(|(acct, _)| acct == &p.account)
                            .map(|(_, d)| d.as_path())
                            .unwrap_or_else(|| config_dirs.first().map(|(_, d)| d.as_path()).unwrap());
                        let sess = filter_sessions_before(
                            filter_sessions_since(
                                discover_sessions(&p.decoded_path, None, config_dir),
                                since,
                            ),
                            before,
                        );
                        for s in &sess {
                            if s.is_subagent { continue; }
                            let smtime: chrono::DateTime<chrono::Utc> = s.mtime.into();
                            println!("  {} {}", smtime.format("%Y-%m-%d %H:%M:%S"), s.session_id);
                        }
                    }
                }
                eprintln!("{} project{}", projects.len(), if projects.len() == 1 { "" } else { "s" });
            }
        }

        Commands::Dump { session, project, targets, no_diff, json, subagents } => {
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

            let mut all_contents = vec![];
            for s in &sessions {
                if !subagents && s.is_subagent {
                    continue;
                }
                all_contents.extend(parser::extract_content_opts(
                    &s.file_path,
                    &target_set,
                    &s.session_id,
                    s.is_subagent,
                    json,
                ));
            }

            all_contents.sort_by(|a, b| a.timestamp.cmp(&b.timestamp));

            for content in &all_contents {
                print_dump_record(content, json, no_diff);
            }
        }

        Commands::Tail { count, follow, session, project, targets, no_diff, json, subagents } => {
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

            let print_content = |content: &parser::ExtractedContent| {
                print_dump_record(content, json, no_diff);
            };

            let mut all_contents = vec![];
            for s in &sessions {
                if !subagents && s.is_subagent {
                    continue;
                }
                all_contents.extend(parser::extract_content_opts(
                    &s.file_path,
                    &target_set,
                    &s.session_id,
                    s.is_subagent,
                    json,
                ));
            }

            all_contents.sort_by(|a, b| a.timestamp.cmp(&b.timestamp));

            let skip = all_contents.len().saturating_sub(count);
            for content in all_contents.into_iter().skip(skip) {
                print_content(&content);
            }

            if follow {
                use std::io::{BufRead, BufReader, Seek, SeekFrom};

                // Follow only the main (non-subagent) session file
                let main_session = match sessions.iter().find(|s| !s.is_subagent) {
                    Some(s) => s,
                    None => {
                        eprintln!("No main session file to follow");
                        std::process::exit(1);
                    }
                };

                let mut file = match std::fs::File::open(&main_session.file_path) {
                    Ok(f) => f,
                    Err(e) => {
                        eprintln!("error: failed to open {}: {}", main_session.file_path.display(), e);
                        std::process::exit(1);
                    }
                };

                // Seek to end — we already printed the initial tail
                file.seek(SeekFrom::End(0)).unwrap();

                let mut tool_use_map = parser::ToolUseMap::new();
                let mut reader = BufReader::new(file);
                let mut line_buf = String::new();

                loop {
                    line_buf.clear();
                    match reader.read_line(&mut line_buf) {
                        Ok(0) => {
                            // EOF — sleep and retry
                            std::thread::sleep(std::time::Duration::from_millis(200));
                            continue;
                        }
                        Ok(_) => {
                            let line = line_buf.trim_end();
                            if line.is_empty() {
                                continue;
                            }
                            match serde_json::from_str::<serde_json::Value>(line) {
                                Ok(entry) => {
                                    parser::collect_tool_use_ids(&entry, &mut tool_use_map);
                                    let mut results = vec![];
                                    parser::extract_from_entry(
                                        &entry,
                                        &tool_use_map,
                                        &target_set,
                                        &main_session.session_id,
                                        main_session.is_subagent,
                                        &mut results,
                                    );
                                    for content in &results {
                                        print_content(content);
                                    }
                                    let _ = std::io::stdout().flush();
                                }
                                Err(_) => {
                                    // Incomplete line (file still being written),
                                    // put it back by seeking backward
                                    let n = line_buf.len() as i64;
                                    let inner = reader.get_mut();
                                    let _ = inner.seek(SeekFrom::Current(-n));
                                    // Clear the BufReader's internal buffer so it
                                    // re-reads from the seeked position
                                    reader = BufReader::new(reader.into_inner());
                                    std::thread::sleep(std::time::Duration::from_millis(200));
                                }
                            }
                        }
                        Err(_) => {
                            std::thread::sleep(std::time::Duration::from_millis(200));
                        }
                    }
                }
            }
        }

        Commands::Memory { subcommand } => {
            let paths: Vec<&Path> = config_dirs.iter().map(|(_, d)| d.as_path()).collect();
            run_memory(subcommand, &paths);
        }
    }
}

fn run_memory(cmd: MemoryCommands, config_dirs: &[&Path]) {
    match cmd {
        MemoryCommands::Dump { project, no_subdirs, json, files_only } => {
            let cwd = resolve_project_path(&project);
            let files = discover_memory_files(&cwd, config_dirs, !no_subdirs);

            if files.is_empty() {
                eprintln!("No memory files found for {}", cwd.display());
                std::process::exit(1);
            }

            if json {
                let arr: Vec<_> = files.iter().map(|f| {
                    let content = std::fs::read_to_string(&f.path).unwrap_or_default();
                    let imported_by = f.imported_by.as_ref().map(|p| p.to_string_lossy().into_owned());
                    json!({
                        "path": f.path.to_string_lossy(),
                        "source": f.source.label(),
                        "importedBy": imported_by,
                        "content": content,
                    })
                }).collect();
                println!("{}", serde_json::to_string_pretty(&arr).unwrap());
                return;
            }

            if files_only {
                for f in &files {
                    println!("{}", f.path.display());
                }
                return;
            }

            let mut first = true;
            for f in &files {
                if !first { println!(); }
                first = false;
                print_memory_header(f);
                match std::fs::read_to_string(&f.path) {
                    Ok(content) => print!("{}", content),
                    Err(e) => eprintln!("warning: failed to read {}: {}", f.path.display(), e),
                }
            }
            eprintln!("\n{} file{}", files.len(), if files.len() == 1 { "" } else { "s" });
        }

        MemoryCommands::Search {
            pattern, project, no_subdirs,
            context, before_context, after_context,
            max_line_width, max_results, json,
            files_with_matches, ignore_case, fixed_strings, extended_regexp,
        } => {
            let cwd = resolve_project_path(&project);
            let files = discover_memory_files(&cwd, config_dirs, !no_subdirs);

            if files.is_empty() {
                eprintln!("No memory files found for {}", cwd.display());
                std::process::exit(1);
            }

            let flags = if ignore_case { "(?i)" } else { "" };
            let escaped = regex::escape(&pattern);
            let patterns: Vec<Regex> = if fixed_strings {
                vec![Regex::new(&format!("{}{}", flags, escaped)).expect("invalid pattern")]
            } else if extended_regexp {
                match Regex::new(&format!("{}{}", flags, pattern)) {
                    Ok(r) => vec![r],
                    Err(e) => {
                        eprintln!("error: invalid regex '{}': {}", pattern, e);
                        std::process::exit(2);
                    }
                }
            } else {
                let literal = Regex::new(&format!("{}{}", flags, escaped)).expect("invalid pattern");
                match Regex::new(&format!("{}{}", flags, pattern)) {
                    Ok(r) => vec![r],
                    Err(_) => vec![literal],
                }
            };

            let ctx = context.unwrap_or(0);
            let ctx_before = before_context.unwrap_or(ctx);
            let ctx_after = after_context.unwrap_or(ctx);

            let mut total = 0usize;
            let mut first_out = true;
            let stdout = std::io::stdout();
            reset_truncation_state();

            for f in &files {
                if total >= max_results { break }
                let content = match std::fs::read_to_string(&f.path) {
                    Ok(c) => c,
                    Err(_) => continue,
                };
                let Some(matched) = find_matches(&content, &patterns, ctx_before, ctx_after) else { continue };

                if files_with_matches {
                    println!("{}", f.path.display());
                    total += 1;
                    continue;
                }

                if json {
                    for (idx, ml) in matched.iter().enumerate() {
                        println!("{}", json!({
                            "path": f.path.to_string_lossy(),
                            "source": f.source.label(),
                            "matchIndex": idx,
                            "isMatch": ml.is_match,
                            "text": ml.line,
                        }));
                    }
                    total += 1;
                    continue;
                }

                let mut out = stdout.lock();
                if !first_out { writeln!(out).unwrap(); }
                first_out = false;
                writeln!(out, "{}", format_memory_match_header(f)).unwrap();
                for ml in &matched {
                    let rendered = format_memory_line(&ml.line, ml.is_match, &patterns, max_line_width);
                    writeln!(out, "{}", rendered).unwrap();
                }
                out.flush().unwrap();
                total += 1;
            }

            if !files_with_matches && !json {
                println!("\n{} file{} with matches of {} scanned", total,
                    if total == 1 { "" } else { "s" }, files.len());
                if total >= max_results {
                    eprintln!("Hint: Result limit reached. Use --max-results to increase the limit.");
                }
                if get_did_truncate() {
                    eprintln!("Hint: Some lines were truncated. Use --max-line-width 0 for full output, or --max-line-width <n> to adjust.");
                }
            }
            if total == 0 { std::process::exit(1); }
        }
    }
}

fn resolve_project_path(p: &Path) -> PathBuf {
    p.canonicalize().unwrap_or_else(|_| p.to_path_buf())
}

fn print_memory_header(f: &MemoryFile) {
    let label = console::style(format!("[{}]", f.source.label())).dim();
    let path = console::style(f.path.display().to_string()).cyan().bold();
    if let Some(ref imp) = f.imported_by {
        let import_hint = console::style(format!("(imported by {})", imp.display())).dim();
        println!("==> {} {} {}", path, label, import_hint);
    } else {
        println!("==> {} {}", path, label);
    }
}

fn format_memory_match_header(f: &MemoryFile) -> String {
    let label = console::style(format!("[{}]", f.source.label())).dim();
    let path = console::style(f.path.display().to_string()).cyan().bold();
    format!("{} {}", path, label)
}

fn format_memory_line(line: &str, is_match: bool, patterns: &[Regex], max_line_width: usize) -> String {
    let marker = if is_match { ">" } else { " " };
    let rendered = output::highlight_matches(line, patterns, max_line_width);
    format!("  {} {}", marker, rendered)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_records_spec_single_positive() {
        assert_eq!(parse_records_spec("5").unwrap(), vec![5]);
    }

    #[test]
    fn test_parse_records_spec_single_negative() {
        assert_eq!(parse_records_spec("-3").unwrap(), vec![-3]);
    }

    #[test]
    fn test_parse_records_spec_range_positive() {
        assert_eq!(parse_records_spec("1..3").unwrap(), vec![1, 2, 3]);
    }

    #[test]
    fn test_parse_records_spec_range_negative() {
        assert_eq!(parse_records_spec("-3..-1").unwrap(), vec![-3, -2, -1]);
    }

    #[test]
    fn test_parse_records_spec_range_across_zero_drops_zero() {
        // Offset 0 (the match itself) is always silently skipped.
        assert_eq!(parse_records_spec("-2..2").unwrap(), vec![-2, -1, 1, 2]);
    }

    #[test]
    fn test_parse_records_spec_comma_list() {
        assert_eq!(parse_records_spec("-3,-1,2,5").unwrap(), vec![-3, -1, 2, 5]);
    }

    #[test]
    fn test_parse_records_spec_mixed_ranges_and_singletons() {
        assert_eq!(parse_records_spec("-3..-1,2,5..6").unwrap(), vec![-3, -2, -1, 2, 5, 6]);
    }

    #[test]
    fn test_parse_records_spec_dedups_overlapping() {
        // Overlapping ranges collapse via BTreeSet.
        assert_eq!(parse_records_spec("1..3,2..4").unwrap(), vec![1, 2, 3, 4]);
    }

    #[test]
    fn test_parse_records_spec_reversed_range_errors() {
        assert!(parse_records_spec("3..1").is_err());
    }

    #[test]
    fn test_parse_records_spec_open_ended_errors() {
        assert!(parse_records_spec("1..").is_err());
        assert!(parse_records_spec("..3").is_err());
    }

    #[test]
    fn test_parse_records_spec_garbage_errors() {
        assert!(parse_records_spec("abc").is_err());
        assert!(parse_records_spec("1..x").is_err());
    }

    #[test]
    fn test_parse_records_spec_only_zero_is_empty() {
        assert_eq!(parse_records_spec("0").unwrap(), Vec::<i32>::new());
    }

    #[test]
    fn test_parse_records_spec_ignores_whitespace_and_blanks() {
        assert_eq!(parse_records_spec(" 1 , , -2 ").unwrap(), vec![-2, 1]);
    }

    #[test]
    fn test_merge_record_context_around() {
        assert_eq!(
            merge_record_context(Some(2), None, None, None).unwrap(),
            vec![-2, -1, 1, 2]
        );
    }

    #[test]
    fn test_merge_record_context_before_only() {
        assert_eq!(
            merge_record_context(None, Some(3), None, None).unwrap(),
            vec![-3, -2, -1]
        );
    }

    #[test]
    fn test_merge_record_context_after_only() {
        assert_eq!(
            merge_record_context(None, None, Some(3), None).unwrap(),
            vec![1, 2, 3]
        );
    }

    #[test]
    fn test_merge_record_context_combines_all_sources() {
        // Shorthand flags and --records SPEC unify into one sorted, deduped set.
        let got = merge_record_context(
            Some(1),          // around 1 -> {-1, 1}
            None,
            Some(3),          // after 3 -> {1, 2, 3}
            Some("-5..-4,7"), // -> {-5, -4, 7}
        ).unwrap();
        assert_eq!(got, vec![-5, -4, -1, 1, 2, 3, 7]);
    }

    #[test]
    fn test_merge_record_context_empty_when_none_set() {
        assert_eq!(
            merge_record_context(None, None, None, None).unwrap(),
            Vec::<i32>::new()
        );
    }
}
