mod parser;
mod sessions;
mod search;
mod output;
mod memory;
mod source;
mod backends;

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use std::io::Write;

use clap::{Parser, Subcommand, ValueEnum};
use regex::Regex;
use serde_json::json;

use crate::source::Source;
use crate::sessions::{resolve_session, ProjectInfo};
use crate::search::{search_sessions, SearchOptions, find_matches};
use crate::output::{format_diff, format_edit_diff, format_match, format_summary, format_project_header, format_multi_summary, reset_truncation_state, get_did_truncate, format_record};
use crate::parser::{QualifiedTarget, Target, TargetSelector};
use crate::memory::MemoryFile;

#[derive(Clone, Copy, Debug, ValueEnum)]
enum ColorWhen {
    /// Colorize output only when writing to a terminal
    Auto,
    /// Always colorize output
    Always,
    /// Never colorize output
    Never,
}

/// Which transcript backend(s) to read from.
#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
enum BackendSel {
    /// Use every backend that has data (Claude JSONL + opencode SQLite + …).
    Auto,
    /// Only the Claude Code JSONL backend.
    Claude,
    /// Only the opencode SQLite backend.
    Opencode,
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

    /// Backend(s) to read: auto (default), claude, or opencode
    #[arg(long, global = true, value_enum, default_value_t = BackendSel::Auto)]
    backend: BackendSel,

    /// Path to the opencode SQLite DB (default: $XDG_DATA_HOME/opencode/opencode.db)
    #[arg(long = "opencode-db", global = true, value_name = "PATH")]
    opencode_db: Option<PathBuf>,

    /// When to use colors: auto, always, never (also respects NO_COLOR env var)
    #[arg(long, global = true, default_value = "auto", value_name = "WHEN")]
    color: ColorWhen,

    /// Only show sessions modified after the given date (git-compatible: yesterday, '2 days ago', '2026-03-24', Monday, 'last week')
    #[arg(long = "after", alias = "since", global = true)]
    after: Option<String>,

    /// Only show sessions modified before the given date (git-compatible: yesterday, '2 days ago', '2026-03-24', Monday, 'last week')
    #[arg(long = "before", alias = "until", global = true)]
    before: Option<String>,

    /// Project path (default: current directory)
    #[arg(long, global = true, default_value = ".")]
    project: PathBuf,

    /// List/search all known projects (rejected by dump/tail/memory)
    #[arg(long = "all-projects", global = true)]
    all_projects: bool,

    /// Filter to projects matching REGEXP, can be repeated (rejected by dump/tail/memory)
    #[arg(short = 'P', long = "project-regexp", value_name = "REGEXP", global = true)]
    project_regexp: Vec<String>,

    /// Specific session: UUID prefix, offset like -1, 0 for latest, or "all"
    #[arg(long, global = true)]
    session: Option<String>,

    /// Content types to include (use TYPE.SUBTYPE for subtype filters; see search --help)
    #[arg(short = 't', long, global = true, default_value = "default", long_help = TARGETS_HELP)]
    targets: String,

    /// Show raw key/value format for Edit tool matches instead of unified diff
    #[arg(long = "no-diff", global = true)]
    no_diff: bool,

    /// Max output line width (0 = unlimited)
    #[arg(long = "max-line-width", global = true, default_value = "200")]
    max_line_width: usize,

    /// Max results for `search`/`memory search` (default: 50; 0 = unlimited; ignored by other subcommands)
    #[arg(long = "max-results", global = true)]
    max_results: Option<usize>,

    /// Include subagent transcripts (where applicable)
    #[arg(long, global = true)]
    subagents: bool,

    /// JSON output
    #[arg(long, global = true)]
    json: bool,

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

        /// Only print session IDs with matches
        #[arg(short = 'l', long = "list", alias = "sessions-with-matches")]
        sessions_with_matches: bool,

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

    /// List sessions for a project
    Sessions {},

    /// Show the last N records across all sessions, sorted by time
    Last {
        /// Number of records to show
        #[arg(short = 'n', long = "last", default_value = "20")]
        count: usize,
    },

    /// List all known projects under ~/.claude/projects/
    Projects {
        /// List sessions within each project
        #[arg(short = 's', long)]
        sessions: bool,
    },

    /// Dump a session's content as plain text
    Dump {
        /// Session ID prefix, offset (e.g. -1 for previous, 0 for latest), or "all" (default: 0)
        #[arg(allow_hyphen_values = true, default_value = "0")]
        session_pos: String,
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
        session_pos: String,
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
        /// Exclude on-demand CLAUDE.md files in subdirectories
        #[arg(long = "no-subdirs")]
        no_subdirs: bool,

        /// Only print the list of discovered file paths
        #[arg(short = 'l', long = "files-only")]
        files_only: bool,
    },

    /// Search markdown memory files that apply to the project
    Search {
        /// Pattern to search (literal string and/or regex)
        pattern: String,

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

        /// Only print file paths with matches
        #[arg(short = 'l', long = "list", alias = "files-with-matches")]
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

/// Subtype-narrowed targets included in `--targets default`. These are bare
/// targets we don't want in defaults wholesale (too noisy), but for which one
/// or two specific subtypes are user-relevant — e.g. `system` is dominated by
/// hook/turn telemetry, but `system.away_summary` (the recap shown when
/// resuming a session) is worth surfacing.
fn default_subtype_filters() -> Vec<(Target, &'static str)> {
    vec![
        (Target::System, "away_summary"),
    ]
}

fn all_targets() -> HashSet<Target> {
    let mut t = default_targets();
    t.insert(Target::System);
    t.insert(Target::FileHistorySnapshot);
    t.insert(Target::LastPrompt);
    t.insert(Target::AgentName);
    t.insert(Target::CustomTitle);
    t.insert(Target::AiTitle);
    t.insert(Target::PermissionMode);
    t.insert(Target::Attachment);
    t.insert(Target::Progress);
    t.insert(Target::PullRequest);
    t.insert(Target::BridgeSession);
    t.insert(Target::Mode);
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

/// Help text for the `--targets` flag, shared across all subcommands.
const TARGETS_HELP: &str = "\
Content types to include (comma-separated). Use TYPE.SUBTYPE to filter further \
(e.g. system.away_summary, tool-use.Edit, attachment.task_reminder, \
pull-request.owner/repo). A bare TYPE matches all subtypes; bare always wins \
over qualified for the same TYPE. Subtypes apply to: tool-use, tool-result, \
bash-output, bash-command, system, progress, attachment, queue-operation, \
pull-request.\n\n\
Types: user, assistant, thinking, bash-command, bash-output, tool-use, \
tool-result, subagent-prompt, compact-summary, system, file-history-snapshot, \
queue-operation, last-prompt, agent-name, custom-title, ai-title, permission-mode, \
attachment, progress, pull-request, bridge-session, mode.\n\n\
Aliases: \"default\" = standard types (also includes system.away_summary recaps), \"all\" = everything.";

fn parse_targets_or_exit(s: &str) -> TargetSelector {
    parse_targets(s).unwrap_or_else(|e| {
        eprintln!("error: {}", e);
        std::process::exit(2);
    })
}

fn parse_targets(s: &str) -> Result<TargetSelector, String> {
    let mut items: Vec<QualifiedTarget> = vec![];
    for tok in s.split(',') {
        let tok = tok.trim();
        match tok {
            "" => continue,
            "default" => {
                for t in default_targets() {
                    items.push(QualifiedTarget::bare(t));
                }
                for (t, sub) in default_subtype_filters() {
                    items.push(QualifiedTarget {
                        target: t,
                        subtype: Some(sub.to_string()),
                    });
                }
                continue;
            }
            "all" => {
                for t in all_targets() {
                    items.push(QualifiedTarget::bare(t));
                }
                continue;
            }
            _ => {}
        }

        match tok.parse::<QualifiedTarget>() {
            Ok(q) => items.push(q),
            Err(msg) => {
                // If the bare TYPE itself is unknown, this is a user typo — fatal.
                // If TYPE is valid but the subtype isn't supported, keep the bare TYPE
                // and just warn (preserves prior behaviour for e.g. `user.foo`).
                let bare_name = tok.split_once('.').map(|(n, _)| n).unwrap_or(tok);
                if bare_name.parse::<Target>().is_err() {
                    return Err(format!("unknown target type '{}': {}", bare_name, msg));
                }
                eprintln!("warning: {}", msg);
                if let Some((name, _)) = tok.split_once('.') {
                    if let Ok(t) = name.parse::<Target>() {
                        items.push(QualifiedTarget::bare(t));
                    }
                }
            }
        }
    }
    Ok(items.into_iter().collect())
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

/// Single project (resolved canonical path) vs multi-project iteration over
/// all known projects, optionally regex-filtered.
enum ProjectScope {
    Single(String),
    Multi(Vec<ProjectInfo>),
}

/// Decide between single-project and multi-project iteration based on the
/// global flags. `--all-projects` or any `--project-regexp` switches to
/// `Multi`; otherwise it's `Single` against the resolved `--project` path.
fn select_project_scope(
    project: &Path,
    all_projects: bool,
    project_regexp: &[String],
    source: &dyn Source,
) -> Result<ProjectScope, String> {
    if !all_projects && project_regexp.is_empty() {
        return Ok(ProjectScope::Single(resolve_project(&project.to_path_buf())));
    }
    let mut compiled: Vec<Regex> = vec![];
    for p in project_regexp {
        match Regex::new(p) {
            Ok(r) => compiled.push(r),
            Err(e) => return Err(format!("invalid project regexp '{}': {}", p, e)),
        }
    }
    let all_infos = source.discover_projects();
    let filtered: Vec<ProjectInfo> = if compiled.is_empty() {
        all_infos
    } else {
        all_infos.into_iter()
            .filter(|p| compiled.iter().any(|r| r.is_match(&p.decoded_path)))
            .collect()
    };
    Ok(ProjectScope::Multi(filtered))
}

/// Discover sessions per project in `scope`, applying date filters and the
/// subagent toggle. Returns one entry per project that has at least one
/// surviving session; outer Vec is length 1 for Single scope.
fn discover_sessions_for_scope(
    scope: &ProjectScope,
    source: &dyn Source,
    since: Option<chrono::DateTime<chrono::Utc>>,
    before: Option<chrono::DateTime<chrono::Utc>>,
    include_subagents: bool,
) -> Vec<(Option<String>, Vec<sessions::SessionFile>)> {
    let collect = |project_path: &str| -> Vec<sessions::SessionFile> {
        let raw = source.discover_sessions(project_path);
        let dated = filter_sessions_before(filter_sessions_since(raw, since), before);
        if include_subagents {
            dated
        } else {
            dated.into_iter().filter(|s| !s.is_subagent).collect()
        }
    };
    match scope {
        ProjectScope::Single(path) => {
            let sessions = collect(path);
            if sessions.is_empty() { vec![] } else { vec![(None, sessions)] }
        }
        ProjectScope::Multi(projects) => projects.iter()
            .filter_map(|p| {
                let sessions = collect(&p.decoded_path);
                if sessions.is_empty() { None } else { Some((Some(p.decoded_path.clone()), sessions)) }
            })
            .collect(),
    }
}

/// Fallback discoverability aid: when a single-project search turns up no
/// session files (the user is typically in a directory claugrep has no history
/// for), scan every known project for the same pattern and report which ones do
/// contain at least one match. Printed to stderr so it never contaminates
/// stdout / piped output.
///
/// Kept cheap by stopping each project at its first hit (`max_results = 1`) and
/// dropping all context extraction — so a project with a match returns almost
/// immediately, and only genuinely non-matching projects pay a full scan.
fn report_projects_with_matches(
    source: &dyn Source,
    options: &SearchOptions,
    since: Option<chrono::DateTime<chrono::Utc>>,
    before: Option<chrono::DateTime<chrono::Utc>>,
) {
    let disc_options = SearchOptions {
        max_results: 1,
        context_before: 0,
        context_after: 0,
        context_offsets: vec![],
        context_type_filter: None,
        // Only need the match targets; skip the wider extraction universe that
        // record-context would otherwise pull in.
        extract_targets: options.targets.targets.clone(),
        json_output: false,
        ..options.clone()
    };

    let scope = ProjectScope::Multi(source.discover_projects());
    let groups = discover_sessions_for_scope(&scope, source, since, before, true);

    // Dedup by project path: multiple backends can surface the same directory
    // as separate groups, and we only want to name each project once.
    let mut seen = std::collections::HashSet::new();
    let mut hits: Vec<String> = Vec::new();
    for (label, sessions) in &groups {
        let Some(l) = label else { continue };
        if !seen.contains(l) {
            let (count, _) = search_sessions(source, sessions, &disc_options, |_| {});
            if count > 0 {
                seen.insert(l.clone());
                hits.push(l.clone());
            }
        }
    }

    if hits.is_empty() {
        eprintln!("No other projects contain matches for this pattern either.");
    } else {
        eprintln!(
            "However, {} other project(s) contain matches for this pattern:",
            hits.len()
        );
        for h in &hits {
            eprintln!("  {}", h);
        }
        eprintln!("Re-run with --all-projects to search across them, or -p <path> to target one.");
    }
}

/// Resolve the session selector for `dump`/`tail`, where the selector can come
/// from the global `--session` flag or from the positional `<SESSION>` arg
/// (default `"0"` = latest). Errors if the user supplied both with non-default
/// values.
fn resolve_session_arg(flag: &Option<String>, positional: &str) -> Result<String, String> {
    match flag {
        Some(f) if positional != "0" => Err(format!(
            "cannot use both positional <SESSION> ('{}') and --session ('{}'); pick one",
            positional, f,
        )),
        Some(f) => Ok(f.clone()),
        None => Ok(positional.to_string()),
    }
}

/// Reject `--all-projects` and `--project-regexp` for subcommands that operate
/// on a single project (dump, tail, memory dump, memory search).
fn validate_multi_project_unsupported(
    cmd_name: &'static str,
    all_projects: bool,
    project_regexp: &[String],
) -> Result<(), String> {
    if all_projects {
        return Err(format!(
            "--all-projects is not supported for the '{}' subcommand (operates on a single project)",
            cmd_name,
        ));
    }
    if !project_regexp.is_empty() {
        return Err(format!(
            "--project-regexp is not supported for the '{}' subcommand (operates on a single project)",
            cmd_name,
        ));
    }
    Ok(())
}

/// Emit a SearchMatch as JSON to stdout.
/// With `wrap_context = false` this preserves the historical stream format
/// (one raw entry per line). With `wrap_context = true` each match is wrapped
/// in `{"match": raw, "context": [{"offset": n, "entry": raw}, ...]}` so the
/// neighboring records the user asked for can be grouped with their match.
/// Default cap used by `search` and `memory search` when the user does not pass
/// `--max-results`. Kept in code rather than in clap's `default_value` so we can
/// distinguish "user didn't set it" from "user explicitly chose this number".
const DEFAULT_MAX_RESULTS: usize = 50;

/// Resolve `--max-results` for subcommands that consume it.
/// `None` → use the default cap; `Some(0)` → unlimited (represented as `usize::MAX`
/// so existing cap arithmetic Just Works); `Some(n>0)` → n.
fn resolve_max_results(opt: Option<usize>) -> usize {
    match opt {
        None => DEFAULT_MAX_RESULTS,
        Some(0) => usize::MAX,
        Some(n) => n,
    }
}

/// Warn (once) when `--max-results` is set on a subcommand that doesn't use it,
/// so the flag's footprint matches its advertised effect.
fn warn_max_results_ignored(opt: Option<usize>, cmd_name: &str) {
    if opt.is_some() {
        eprintln!("warning: --max-results has no effect on the '{}' subcommand", cmd_name);
    }
}

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

fn print_dump_record(
    content: &parser::ExtractedContent,
    json: bool,
    no_diff: bool,
    max_line_width: usize,
) {
    if json {
        if let Some(ref raw) = content.raw_entry {
            println!("{}", raw);
        }
        return;
    }
    let label = format!("[{}]", QualifiedTarget {
        target: content.target.clone(),
        subtype: content.tool_name.clone(),
    });
    let label = console::style(label).dim();
    if !no_diff {
        if let Some(ref diff) = content.edit_diff {
            println!("{}\n{}", label, format_edit_diff(diff));
            return;
        }
    }
    let truncated: String = content.text.split('\n')
        .map(|line| output::truncate_line(line, &[], max_line_width).0)
        .collect::<Vec<_>>()
        .join("\n");
    let sep = if truncated.contains('\n') { "\n" } else { " " };
    println!("{}{}{}", label, sep, truncated);
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

    // Build the active backend set. `--backend` selects; `auto` (default)
    // enables every backend that has a usable store.
    let claude_config_dirs = effective_config_dirs(cli.config_dir.as_ref(), cli.account.as_deref());
    let mut children: Vec<Box<dyn Source>> = Vec::new();
    if !matches!(cli.backend, BackendSel::Opencode) {
        children.push(Box::new(backends::claude::ClaudeSource::new(claude_config_dirs.clone())));
    }
    if matches!(cli.backend, BackendSel::Auto | BackendSel::Opencode) {
        let db = cli.opencode_db.clone()
            .or_else(backends::opencode::OpenCodeSource::default_db_path);
        match db {
            Some(db) => children.push(Box::new(backends::opencode::OpenCodeSource::new(db))),
            None if matches!(cli.backend, BackendSel::Opencode) => {
                eprintln!("error: --backend opencode selected but no opencode.db found; pass --opencode-db <path>");
                std::process::exit(2);
            }
            None => {}
        }
    }
    let multi = source::MultiSource::new(children);
    let source: &dyn Source = &multi;

    let since: Option<chrono::DateTime<chrono::Utc>> = match cli.after.as_deref() {
        None => None,
        Some(v) => match parse_since_date(v) {
            Ok(dt) => Some(dt),
            Err(e) => {
                eprintln!("error: {}", e);
                std::process::exit(2);
            }
        },
    };

    let before: Option<chrono::DateTime<chrono::Utc>> = match cli.before.as_deref() {
        None => None,
        Some(v) => match parse_since_date(v) {
            Ok(dt) => Some(dt),
            Err(e) => {
                eprintln!("error: {}", e);
                std::process::exit(2);
            }
        },
    };

    let Cli {
        command,
        config_dir: _, account: _, color: _, after: _, before: _,
        backend: _, opencode_db: _,
        project,
        all_projects,
        project_regexp,
        session: cli_session,
        targets: targets_str,
        no_diff,
        max_line_width,
        max_results: max_results_opt,
        subagents,
        json,
    } = cli;

    match command {
        Commands::Search {
            pattern, context, before_context, after_context,
            around_records, before_records, after_records, records, records_type,
            sessions_with_matches, ignore_case,
            fixed_strings, extended_regexp,
        } => {
            let max_results = resolve_max_results(max_results_opt);
            let targets = parse_targets_or_exit(&targets_str);

            let context_offsets = match merge_record_context(
                around_records, before_records, after_records, records.as_deref(),
            ) {
                Ok(v) => v,
                Err(e) => {
                    eprintln!("error: {}", e);
                    std::process::exit(2);
                }
            };
            let context_type_filter: Option<TargetSelector> = records_type
                .as_deref()
                .map(parse_targets_or_exit);

            // When record-context is requested, extract all known types so offsets
            // can walk over any record; otherwise just the -t targets are enough.
            let extract_targets: HashSet<Target> = if context_offsets.is_empty() {
                targets.targets.clone()
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
                match Regex::new(&format!("{}{}", flags, pattern)) {
                    Ok(regex_pat) => vec![regex_pat],
                    Err(e) => {
                        eprintln!("warning: pattern '{}' is not a valid regex ({}); falling back to literal search. Use -F to silence, or -E to make this an error.", pattern, e);
                        vec![literal_pat]
                    }
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

            let scope = match select_project_scope(&project, all_projects, &project_regexp, source) {
                Ok(s) => s,
                Err(e) => {
                    eprintln!("error: {}", e);
                    std::process::exit(2);
                }
            };
            let is_multi = matches!(scope, ProjectScope::Multi(_));

            if let ProjectScope::Multi(ref projs) = scope {
                if projs.is_empty() {
                    eprintln!("No projects matched the given filters");
                    std::process::exit(1);
                }
            }

            // Search always considers subagent files (target-driven); ignore the global flag here.
            let session_groups = discover_sessions_for_scope(&scope, source, since, before, true);

            if !is_multi && session_groups.is_empty() {
                let project_path = match &scope {
                    ProjectScope::Single(p) => p.clone(),
                    _ => String::new(),
                };
                eprintln!("No session files found for project {}", project_path);
                report_projects_with_matches(source, &options, since, before);
                std::process::exit(1);
            }

            let stdout = std::io::stdout();
            let mut total_matches = 0usize;
            let mut total_sessions_searched = 0usize;
            let mut projects_with_results = 0usize;
            let mut first_project_output = true;
            let mut remaining = max_results;
            let mut any_hit_cap = false;
            let total_projects_searched = match &scope {
                ProjectScope::Single(_) => 1,
                ProjectScope::Multi(p) => p.len(),
            };

            reset_truncation_state();

            if json {
                for (_, sessions) in &session_groups {
                    if remaining == 0 { break; }
                    let resolved = match resolve_session(cli_session.as_deref(), sessions) {
                        Ok(s) => s,
                        Err(e) => {
                            if is_multi { continue; }
                            eprintln!("error: {}", e);
                            std::process::exit(2);
                        }
                    };
                    let proj_options = SearchOptions { max_results: remaining, ..options.clone() };
                    let wrap_context = !proj_options.context_offsets.is_empty();
                    // JSON output suppresses human hints, so hit_cap is unused here.
                    let (count, _hit_cap) = search_sessions(source, &resolved, &proj_options, |m| {
                        emit_json_match(&m, wrap_context);
                    });
                    remaining = remaining.saturating_sub(count);
                }
            } else if sessions_with_matches {
                let mut seen = std::collections::HashSet::new();
                for (_, sessions) in &session_groups {
                    if remaining == 0 { break; }
                    let resolved = match resolve_session(cli_session.as_deref(), sessions) {
                        Ok(s) => s,
                        Err(e) => {
                            if is_multi { continue; }
                            eprintln!("error: {}", e);
                            std::process::exit(2);
                        }
                    };
                    let proj_options = SearchOptions { max_results: remaining, ..options.clone() };
                    // -l mode prints only paths; hit_cap goes unused for now.
                    let (count, _hit_cap) = search_sessions(source, &resolved, &proj_options, |m| {
                        let path = resolved.iter()
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
                for (project_label, sessions) in &session_groups {
                    if remaining == 0 { break; }
                    let resolved = match resolve_session(cli_session.as_deref(), sessions) {
                        Ok(s) => s,
                        Err(e) => {
                            if is_multi { continue; }
                            eprintln!("error: {}", e);
                            std::process::exit(2);
                        }
                    };
                    total_sessions_searched += resolved.len();

                    // Buffer per-project results so we only emit a project header
                    // (multi mode) when at least one match landed.
                    let mut project_lines: Vec<String> = vec![];
                    let mut first_in_proj = true;
                    let proj_options = SearchOptions { max_results: remaining, ..options.clone() };
                    let (count, hit_cap) = search_sessions(source, &resolved, &proj_options, |m| {
                        if !first_in_proj { project_lines.push(String::new()); }
                        first_in_proj = false;
                        let rendered = if !no_diff && m.edit_diff.is_some() {
                            format_diff(&m, m.edit_diff.as_ref().unwrap(), &patterns, max_line_width, diff_ctx)
                        } else {
                            format_match(&m, &patterns, max_line_width)
                        };
                        project_lines.push(rendered);
                    });
                    if hit_cap { any_hit_cap = true; }

                    if count > 0 {
                        let mut out = stdout.lock();
                        if !first_project_output { writeln!(out).unwrap(); }
                        first_project_output = false;
                        if let Some(label) = project_label {
                            writeln!(out, "{}", format_project_header(label)).unwrap();
                            writeln!(out).unwrap();
                        }
                        for line in &project_lines {
                            writeln!(out, "{}", line).unwrap();
                        }
                        out.flush().unwrap();
                        projects_with_results += 1;
                    }
                    total_matches += count;
                    remaining = remaining.saturating_sub(count);
                }

                if is_multi {
                    println!("{}", format_multi_summary(total_matches, projects_with_results, total_projects_searched, total_sessions_searched));
                } else {
                    let project_path = match &scope {
                        ProjectScope::Single(p) => p.as_str(),
                        _ => "",
                    };
                    println!("{}", format_summary(total_matches, project_path, total_sessions_searched));
                }
                if any_hit_cap {
                    eprintln!("Hint: Result limit reached. Use --max-results <n> to raise it, or --max-results 0 for unlimited.");
                }
                if get_did_truncate() {
                    eprintln!("Hint: Some lines were truncated. Use --max-line-width 0 for full output, or --max-line-width <n> to adjust.");
                }
            }
        }

        Commands::Last { count } => {
            warn_max_results_ignored(max_results_opt, "last");
            let target_set = parse_targets_or_exit(&targets_str);

            let scope = match select_project_scope(&project, all_projects, &project_regexp, source) {
                Ok(s) => s,
                Err(e) => {
                    eprintln!("error: {}", e);
                    std::process::exit(2);
                }
            };
            let session_groups = discover_sessions_for_scope(&scope, source, since, before, subagents);

            let all_sessions: Vec<sessions::SessionFile> = session_groups.into_iter()
                .flat_map(|(_, s)| s)
                .collect();

            if all_sessions.is_empty() {
                eprintln!("No session files found");
                std::process::exit(1);
            }

            // Collect all content across all sessions
            let mut all_records: Vec<parser::ExtractedContent> = vec![];
            for session in &all_sessions {
                let contents = source.extract_content(
                    session,
                    &target_set.targets,
                    json,
                );
                all_records.extend(contents);
            }
            all_records.retain(|c| target_set.matches(&c.target, c.tool_name.as_deref()));

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

        Commands::Sessions {} => {
            warn_max_results_ignored(max_results_opt, "sessions");
            let scope = match select_project_scope(&project, all_projects, &project_regexp, source) {
                Ok(s) => s,
                Err(e) => {
                    eprintln!("error: {}", e);
                    std::process::exit(2);
                }
            };
            let is_multi = matches!(scope, ProjectScope::Multi(_));
            let session_groups = discover_sessions_for_scope(&scope, source, since, before, subagents);

            if session_groups.is_empty() {
                let label = match &scope {
                    ProjectScope::Single(p) => format!(" for project {}", p),
                    ProjectScope::Multi(_) => String::new(),
                };
                eprintln!("No sessions found{}", label);
                std::process::exit(1);
            }

            if json {
                let output: serde_json::Value = if is_multi {
                    let arr: Vec<_> = session_groups.iter().map(|(label, sessions)| {
                        let sess_json: Vec<_> = sessions.iter().map(|s| {
                            let mtime = s.mtime.duration_since(std::time::UNIX_EPOCH)
                                .map(|d| d.as_secs()).unwrap_or(0);
                            json!({
                                "sessionId": s.session_id,
                                "filePath": s.file_path.to_string_lossy(),
                                "mtime": mtime,
                                "isSubagent": s.is_subagent,
                                "backend": s.backend,
                            })
                        }).collect();
                        json!({
                            "project": label.as_deref().unwrap_or(""),
                            "sessions": sess_json,
                        })
                    }).collect();
                    json!(arr)
                } else {
                    let sessions = &session_groups[0].1;
                    let arr: Vec<_> = sessions.iter().map(|s| {
                        let mtime = s.mtime.duration_since(std::time::UNIX_EPOCH)
                            .map(|d| d.as_secs()).unwrap_or(0);
                        json!({
                            "sessionId": s.session_id,
                            "filePath": s.file_path.to_string_lossy(),
                            "mtime": mtime,
                            "isSubagent": s.is_subagent,
                            "backend": s.backend,
                        })
                    }).collect();
                    json!(arr)
                };
                println!("{}", serde_json::to_string_pretty(&output).unwrap());
            } else {
                let mut total = 0;
                let mut first_proj = true;
                for (project_label, sessions) in &session_groups {
                    if is_multi {
                        if !first_proj { println!(); }
                        first_proj = false;
                        if let Some(label) = project_label {
                            println!("{}", format_project_header(label));
                        }
                    }
                    for s in sessions {
                        let mtime: chrono::DateTime<chrono::Utc> = s.mtime.into();
                        let suffix = if s.is_subagent { " [subagent]" } else { "" };
                        println!("{} {}{}", mtime.format("%Y-%m-%d %H:%M:%S"), s.session_id, suffix);
                        total += 1;
                    }
                }
                eprintln!("{} session{}", total, if total == 1 { "" } else { "s" });
            }
        }

        Commands::Projects { sessions: list_sessions } => {
            warn_max_results_ignored(max_results_opt, "projects");
            let projects = source.discover_projects();

            if projects.is_empty() {
                eprintln!("No projects found");
                std::process::exit(1);
            }

            // Annotate with [backend] only when more than one backend is
            // present (the common single-backend case stays uncluttered), and
            // with [account] only for multi-account Claude setups.
            let backends: HashSet<&str> = projects.iter().map(|p| p.backend).collect();
            let multi_backend = backends.len() > 1;
            let accounts: HashSet<Option<&str>> = projects.iter().map(|p| p.account.as_deref()).collect();
            let multi_account = accounts.len() > 1;

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
                        "backend": p.backend,
                    });
                    if list_sessions {
                        let sess = filter_sessions_before(
                            filter_sessions_since(
                                source.discover_sessions(&p.decoded_path),
                                since,
                            ),
                            before,
                        );
                        let sess_json: Vec<_> = sess.iter()
                            .filter(|s| subagents || !s.is_subagent)
                            .map(|s| {
                                let smtime = s.mtime.duration_since(std::time::UNIX_EPOCH)
                                    .map(|d| d.as_secs()).unwrap_or(0);
                                json!({
                                    "sessionId": s.session_id,
                                    "filePath": s.file_path.to_string_lossy(),
                                    "mtime": smtime,
                                    "isSubagent": s.is_subagent,
                                    "backend": s.backend,
                                })
                            })
                            .collect();
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
                    let mut tags = String::new();
                    if multi_backend {
                        tags.push_str(&format!(" [{}]", p.backend));
                    }
                    if multi_account {
                        match &p.account {
                            Some(a) => tags.push_str(&format!(" [{}]", a)),
                            None => tags.push_str(" [default]"),
                        }
                    }
                    println!("{} ({} session{}) {}{}",
                        p.decoded_path,
                        p.session_count,
                        if p.session_count == 1 { "" } else { "s" },
                        ts_str,
                        tags);
                    if list_sessions {
                        let sess = filter_sessions_before(
                            filter_sessions_since(
                                source.discover_sessions(&p.decoded_path),
                                since,
                            ),
                            before,
                        );
                        for s in &sess {
                            if !subagents && s.is_subagent { continue; }
                            let smtime: chrono::DateTime<chrono::Utc> = s.mtime.into();
                            let mut suffix = String::new();
                            if s.is_subagent { suffix.push_str(" [subagent]"); }
                            if multi_backend { suffix.push_str(&format!(" [{}]", s.backend)); }
                            println!("  {} {}{}", smtime.format("%Y-%m-%d %H:%M:%S"), s.session_id, suffix);
                        }
                    }
                }
                eprintln!("{} project{}", projects.len(), if projects.len() == 1 { "" } else { "s" });
            }
        }

        Commands::Dump { session_pos } => {
            warn_max_results_ignored(max_results_opt, "dump");
            if let Err(e) = validate_multi_project_unsupported("dump", all_projects, &project_regexp) {
                eprintln!("error: {}", e);
                std::process::exit(2);
            }
            let session = match resolve_session_arg(&cli_session, &session_pos) {
                Ok(s) => s,
                Err(e) => {
                    eprintln!("error: {}", e);
                    std::process::exit(2);
                }
            };
            let project_path = resolve_project(&project);
            let target_set = parse_targets_or_exit(&targets_str);

            let all_sessions = filter_sessions_before(
                filter_sessions_since(
                    source.discover_sessions(&project_path),
                    since,
                ),
                before,
            );
            let sessions = match resolve_session(Some(&session), &all_sessions) {
                Ok(s) => s,
                Err(e) => {
                    eprintln!("error: {}", e);
                    std::process::exit(2);
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
                all_contents.extend(source.extract_content(
                    s,
                    &target_set.targets,
                    json,
                ));
            }
            all_contents.retain(|c| target_set.matches(&c.target, c.tool_name.as_deref()));

            all_contents.sort_by(|a, b| a.timestamp.cmp(&b.timestamp));

            for content in &all_contents {
                print_dump_record(content, json, no_diff, max_line_width);
            }
        }

        Commands::Tail { count, follow, session_pos } => {
            warn_max_results_ignored(max_results_opt, "tail");
            if let Err(e) = validate_multi_project_unsupported("tail", all_projects, &project_regexp) {
                eprintln!("error: {}", e);
                std::process::exit(2);
            }
            let session = match resolve_session_arg(&cli_session, &session_pos) {
                Ok(s) => s,
                Err(e) => {
                    eprintln!("error: {}", e);
                    std::process::exit(2);
                }
            };
            let project_path = resolve_project(&project);
            let target_set = parse_targets_or_exit(&targets_str);

            let all_sessions = filter_sessions_before(
                filter_sessions_since(
                    source.discover_sessions(&project_path),
                    since,
                ),
                before,
            );
            let sessions = match resolve_session(Some(&session), &all_sessions) {
                Ok(s) => s,
                Err(e) => {
                    eprintln!("error: {}", e);
                    std::process::exit(2);
                }
            };

            if sessions.is_empty() {
                eprintln!("No sessions found matching '{}'", session);
                std::process::exit(1);
            }

            let print_content = |content: &parser::ExtractedContent| {
                print_dump_record(content, json, no_diff, max_line_width);
            };

            let mut all_contents = vec![];
            for s in &sessions {
                if !subagents && s.is_subagent {
                    continue;
                }
                all_contents.extend(source.extract_content(
                    s,
                    &target_set.targets,
                    json,
                ));
            }
            all_contents.retain(|c| target_set.matches(&c.target, c.tool_name.as_deref()));

            all_contents.sort_by(|a, b| a.timestamp.cmp(&b.timestamp));

            let skip = all_contents.len().saturating_sub(count);
            for content in all_contents.into_iter().skip(skip) {
                print_content(&content);
            }

            if follow {
                if subagents {
                    eprintln!("warning: --subagents has no effect with -f; only the main session is followed");
                }

                // Follow only the main (non-subagent) session.
                let main_session = match sessions.iter().find(|s| !s.is_subagent) {
                    Some(s) => s.clone(),
                    None => {
                        eprintln!("error: No main session to follow");
                        std::process::exit(2);
                    }
                };

                // The backend's follow primitive yields newly-arrived records
                // (already target-extracted); we apply the subtype selector and
                // print. Blocking until interrupted (SIGPIPE/SIGINT handled by
                // the caller's signal setup).
                let targets_for_follow = target_set.targets.clone();
                let result = source.follow(&main_session, &targets_for_follow, &mut |records| {
                    for content in records {
                        if target_set.matches(&content.target, content.tool_name.as_deref()) {
                            print_content(content);
                        }
                    }
                });
                if let Err(e) = result {
                    eprintln!("error: follow failed: {}", e);
                    std::process::exit(2);
                }
            }
        }

        Commands::Memory { subcommand } => {
            if let Err(e) = validate_multi_project_unsupported("memory", all_projects, &project_regexp) {
                eprintln!("error: {}", e);
                std::process::exit(2);
            }
            // `memory search` honors --max-results; `memory dump` does not.
            let max_results = match &subcommand {
                MemoryCommands::Search { .. } => resolve_max_results(max_results_opt),
                MemoryCommands::Dump { .. } => {
                    warn_max_results_ignored(max_results_opt, "memory dump");
                    DEFAULT_MAX_RESULTS
                }
            };
            let memory_args = MemoryArgs {
                project: &project,
                json,
                max_line_width,
                max_results,
            };
            run_memory(subcommand, source, &memory_args);
        }
    }
}

struct MemoryArgs<'a> {
    project: &'a Path,
    json: bool,
    max_line_width: usize,
    max_results: usize,
}

fn run_memory(cmd: MemoryCommands, source: &dyn Source, args: &MemoryArgs) {
    match cmd {
        MemoryCommands::Dump { no_subdirs, files_only } => {
            let cwd = resolve_project_path(args.project);
            let files = source.discover_memory_files(&cwd, !no_subdirs);

            if files.is_empty() {
                eprintln!("No memory files found for {}", cwd.display());
                std::process::exit(1);
            }

            if args.json {
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
            pattern, no_subdirs,
            context, before_context, after_context,
            files_with_matches, ignore_case, fixed_strings, extended_regexp,
        } => {
            let cwd = resolve_project_path(args.project);
            let files = source.discover_memory_files(&cwd, !no_subdirs);

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
                    Err(e) => {
                        eprintln!("warning: pattern '{}' is not a valid regex ({}); falling back to literal search. Use -F to silence, or -E to make this an error.", pattern, e);
                        vec![literal]
                    }
                }
            };

            let ctx = context.unwrap_or(0);
            let ctx_before = before_context.unwrap_or(ctx);
            let ctx_after = after_context.unwrap_or(ctx);

            let mut total = 0usize;
            let mut hit_cap = false;
            let mut first_out = true;
            let stdout = std::io::stdout();
            reset_truncation_state();

            for f in &files {
                if total >= args.max_results { hit_cap = true; break }
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

                if args.json {
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
                    let rendered = format_memory_line(&ml.line, ml.is_match, &patterns, args.max_line_width);
                    writeln!(out, "{}", rendered).unwrap();
                }
                out.flush().unwrap();
                total += 1;
            }

            if !files_with_matches && !args.json {
                println!("\n{} file{} with matches of {} scanned", total,
                    if total == 1 { "" } else { "s" }, files.len());
                if hit_cap {
                    eprintln!("Hint: Result limit reached. Use --max-results <n> to raise it, or --max-results 0 for unlimited.");
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
