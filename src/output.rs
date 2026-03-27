use crate::search::SearchMatch;
use crate::parser::ExtractedContent;
use regex::Regex;
use console::style;
use similar::ChangeTag;

thread_local! {
    static DID_TRUNCATE: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

pub fn reset_truncation_state() {
    DID_TRUNCATE.with(|f| f.set(false));
}

pub fn get_did_truncate() -> bool {
    DID_TRUNCATE.with(|f| f.get())
}

fn format_timestamp(ts: &str) -> String {
    if ts.is_empty() {
        return "unknown".to_string();
    }
    // Try to parse ISO 8601 and reformat
    ts.replace('T', " ")
        .split('.')
        .next()
        .unwrap_or(ts)
        .trim_end_matches('Z')
        .to_string()
}

fn first_match_pos(line: &str, patterns: &[Regex]) -> Option<(usize, usize)> {
    patterns.iter().filter_map(|p| {
        p.find(line).map(|m| (m.start(), m.len()))
    }).min_by_key(|(start, _)| *start)
}

fn truncate_line(line: &str, patterns: &[Regex], max_width: usize) -> (String, bool) {
    if max_width == 0 || line.len() <= max_width {
        return (line.to_string(), false);
    }

    DID_TRUNCATE.with(|f| f.set(true));

    if let Some((match_start, match_len)) = first_match_pos(line, patterns) {
        let budget = max_width.saturating_sub(match_len);
        let before = budget / 2;
        let after = budget - before;

        let start = {
            let s = match_start.saturating_sub(before);
            // Walk back to the nearest char boundary
            (0..=s).rev().find(|&i| line.is_char_boundary(i)).unwrap_or(0)
        };
        let end = {
            let e = (match_start + match_len + after).min(line.len());
            // Walk forward to the nearest char boundary
            (e..=line.len()).find(|&i| line.is_char_boundary(i)).unwrap_or(line.len())
        };

        let prefix = if start > 0 { "..." } else { "" };
        let suffix = if end < line.len() { "..." } else { "" };
        (format!("{}{}{}", prefix, &line[start..end], suffix), true)
    } else {
        let end = (0..=max_width).rev().find(|&i| line.is_char_boundary(i)).unwrap_or(0);
        (format!("{}...", &line[..end]), true)
    }
}

fn highlight_matches(line: &str, patterns: &[Regex], max_width: usize) -> String {
    let (truncated, _) = truncate_line(line, patterns, max_width);
    let mut result = truncated;
    for p in patterns {
        result = p.replace_all(&result, |caps: &regex::Captures| {
            style(&caps[0]).bold().yellow().to_string()
        }).to_string();
    }
    result
}

/// Color a diff line's content: match spans in bold yellow, non-match spans in base color
/// (red for old/deleted lines, green for new/inserted lines). When colors are disabled,
/// returns plain text.
fn color_diff_line(content: &str, patterns: &[Regex], is_old: bool) -> String {
    // Collect all pattern match spans
    let mut spans: Vec<(usize, usize)> = vec![];
    for p in patterns {
        for m in p.find_iter(content) {
            spans.push((m.start(), m.end()));
        }
    }

    if spans.is_empty() {
        return if is_old {
            style(content).red().to_string()
        } else {
            style(content).green().to_string()
        };
    }

    // Sort and merge overlapping spans
    spans.sort_by_key(|&(s, _)| s);
    let mut merged: Vec<(usize, usize)> = vec![];
    for (start, end) in spans {
        if let Some(last) = merged.last_mut() {
            if start <= last.1 {
                last.1 = last.1.max(end);
                continue;
            }
        }
        merged.push((start, end));
    }

    let mut result = String::new();
    let mut pos = 0;
    for (start, end) in merged {
        if pos < start {
            let seg = &content[pos..start];
            result.push_str(
                &if is_old { style(seg).red().to_string() } else { style(seg).green().to_string() }
            );
        }
        result.push_str(&style(&content[start..end]).bold().yellow().to_string());
        pos = end;
    }
    if pos < content.len() {
        let seg = &content[pos..];
        result.push_str(
            &if is_old { style(seg).red().to_string() } else { style(seg).green().to_string() }
        );
    }
    result
}

/// Render an Edit tool call as a unified diff block using `similar`.
/// context_lines controls how many equal lines appear around changed hunks (default 3).
pub fn format_diff(
    m: &SearchMatch,
    diff: &crate::parser::EditDiff,
    patterns: &[Regex],
    max_line_width: usize,
    context_lines: usize,
) -> String {
    let short_session = &m.session_id[..m.session_id.len().min(8)];
    let time = format_timestamp(&m.timestamp);
    let header = style(format!(
        "--- Match #{} | session={} | {} | tool-use ---",
        m.match_number, short_session, time
    )).cyan().to_string();

    let display_path = diff.file_path.trim_start_matches('/');
    let mut lines = vec![header];
    lines.push(style("tool: Edit").dim().to_string());
    lines.push(style(format!("--- a/{}", display_path)).red().to_string());
    lines.push(style(format!("+++ b/{}", display_path)).green().to_string());

    let text_diff = similar::TextDiff::from_lines(&diff.old_string, &diff.new_string);
    let mut udiff = text_diff.unified_diff();
    udiff.context_radius(context_lines);

    for hunk in udiff.iter_hunks() {
        lines.push(style(format!("{}", hunk.header())).dim().to_string());
        for change in hunk.iter_changes() {
            let line_content = change.value().trim_end_matches('\n');
            let (truncated, _) = truncate_line(line_content, patterns, max_line_width);
            match change.tag() {
                ChangeTag::Delete => {
                    lines.push(format!(
                        "{}{}",
                        style("-").red(),
                        color_diff_line(&truncated, patterns, true)
                    ));
                }
                ChangeTag::Insert => {
                    lines.push(format!(
                        "{}{}",
                        style("+").green(),
                        color_diff_line(&truncated, patterns, false)
                    ));
                }
                ChangeTag::Equal => {
                    lines.push(format!(" {}", truncated));
                }
            }
        }
    }

    lines.join("\n")
}

pub fn format_match(m: &SearchMatch, patterns: &[Regex], max_width: usize) -> String {
    let short_session = &m.session_id[..m.session_id.len().min(8)];
    let time = format_timestamp(&m.timestamp);
    let header = style(format!(
        "--- Match #{} | session={} | {} | {} ---",
        m.match_number, short_session, time, m.target.as_str()
    )).cyan().to_string();

    let mut lines = vec![header];

    if let Some(ref tool) = m.tool_name {
        lines.push(style(format!("tool: {}", tool)).dim().to_string());
    }

    for ml in &m.matched_lines {
        let prefix = if ml.is_match { "> " } else { "  " };
        let content = if ml.is_match {
            highlight_matches(&ml.line, patterns, max_width)
        } else {
            truncate_line(&ml.line, patterns, max_width).0
        };
        lines.push(format!("{}{}", prefix, content));
    }

    lines.join("\n")
}

pub fn format_record(r: &ExtractedContent, max_width: usize) -> String {
    let short_session = &r.session_id[..r.session_id.len().min(8)];
    let time = format_timestamp(&r.timestamp);
    let target = r.target.as_str();
    let tool_suffix = r.tool_name.as_deref().map(|t| format!(":{}", t)).unwrap_or_default();
    let header = style(format!(
        "--- session={} | {} | {}{} ---",
        short_session, time, target, tool_suffix
    )).cyan().to_string();

    let text = if max_width == 0 || r.text.len() <= max_width {
        r.text.clone()
    } else {
        DID_TRUNCATE.with(|f| f.set(true));
        format!("{}...", &r.text[..max_width])
    };

    // Show first line only with ellipsis if multiline
    let display = if let Some(nl) = text.find('\n') {
        let first = &text[..nl];
        let remaining = text[nl+1..].lines().count();
        if remaining > 0 {
            format!("{} {}", first, style(format!("[+{} more lines]", remaining)).dim())
        } else {
            first.to_string()
        }
    } else {
        text
    };

    format!("{}\n{}", header, display)
}

pub fn format_project_header(project_path: &str) -> String {
    style(format!("━━━ {} ━━━", project_path)).bold().blue().to_string()
}

pub fn format_multi_summary(total_matches: usize, projects_with_results: usize, total_projects_searched: usize, total_sessions: usize) -> String {
    let proj_info = style(format!(
        "Searched {} session{} across {} project{} ({} with matches)",
        total_sessions,
        if total_sessions == 1 { "" } else { "s" },
        total_projects_searched,
        if total_projects_searched == 1 { "" } else { "s" },
        projects_with_results,
    )).dim().to_string();

    if total_matches == 0 {
        format!("{}\nNo matches found.", proj_info)
    } else {
        let match_line = style(format!(
            "{} match{} found.",
            total_matches,
            if total_matches == 1 { "" } else { "es" }
        )).dim().to_string();
        format!("\n{}\n{}", proj_info, match_line)
    }
}

pub fn format_summary(count: usize, project_path: &str, session_count: usize) -> String {
    let project_info = style(format!(
        "Searched {} session{} for project {}",
        session_count,
        if session_count == 1 { "" } else { "s" },
        project_path
    )).dim().to_string();

    if count == 0 {
        format!("{}\nNo matches found.", project_info)
    } else {
        let match_line = style(format!(
            "{} match{} found.",
            count,
            if count == 1 { "" } else { "es" }
        )).dim().to_string();
        format!("\n{}\n{}", project_info, match_line)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_format_timestamp_iso() {
        assert_eq!(format_timestamp("2024-01-15T10:30:00.000Z"), "2024-01-15 10:30:00");
    }

    #[test]
    fn test_format_timestamp_empty() {
        assert_eq!(format_timestamp(""), "unknown");
    }

    #[test]
    fn test_truncate_line_no_truncation_needed() {
        let pat = regex::Regex::new("foo").unwrap();
        let (result, was_truncated) = truncate_line("short line", &[pat], 200);
        assert_eq!(result, "short line");
        assert!(!was_truncated);
    }

    #[test]
    fn test_truncate_line_unlimited() {
        let long = "x".repeat(500);
        let pat = regex::Regex::new("x").unwrap();
        let (result, was_truncated) = truncate_line(&long, &[pat], 0);
        assert_eq!(result.len(), 500);
        assert!(!was_truncated);
    }

    #[test]
    fn test_truncate_line_centers_on_match() {
        let line = format!("{}MATCH{}", "a".repeat(100), "b".repeat(100));
        let pat = regex::Regex::new("MATCH").unwrap();
        let (result, was_truncated) = truncate_line(&line, &[pat], 50);
        assert!(was_truncated);
        assert!(result.contains("MATCH"));
        assert!(result.len() <= 50 + 6); // budget + "..." prefixes
    }

    #[test]
    fn test_truncation_state_tracking() {
        reset_truncation_state();
        assert!(!get_did_truncate());
        let long = format!("{}needle{}", "a".repeat(300), "b".repeat(300));
        let pat = regex::Regex::new("needle").unwrap();
        truncate_line(&long, &[pat], 100);
        assert!(get_did_truncate());
        reset_truncation_state();
        assert!(!get_did_truncate());
    }

    fn strip_ansi(s: &str) -> String {
        let re = regex::Regex::new(r"\x1b\[[0-9;]*m").unwrap();
        re.replace_all(s, "").to_string()
    }

    fn make_match(edit_diff: Option<crate::parser::EditDiff>) -> SearchMatch {
        SearchMatch {
            match_number: 1,
            session_id: "test-session-id".to_string(),
            timestamp: "2024-01-01T00:00:00Z".to_string(),
            target: crate::parser::Target::ToolUse,
            tool_name: Some("Edit".to_string()),
            text: String::new(),
            matched_lines: vec![],
            edit_diff,
        }
    }

    #[test]
    fn test_format_diff_contains_headers() {
        let diff = crate::parser::EditDiff {
            file_path: "src/lib.rs".to_string(),
            old_string: "fn old() {}".to_string(),
            new_string: "fn new() {}".to_string(),
        };
        let m = make_match(Some(diff));
        let pat = regex::Regex::new("old").unwrap();
        let output = strip_ansi(&format_diff(&m, m.edit_diff.as_ref().unwrap(), &[pat], 200, 3));
        assert!(output.contains("--- a/src/lib.rs"), "should have --- header");
        assert!(output.contains("+++ b/src/lib.rs"), "should have +++ header");
        assert!(output.contains("-fn old() {}"), "should show removed line");
        assert!(output.contains("+fn new() {}"), "should show added line");
    }

    #[test]
    fn test_format_diff_hunk_header() {
        let diff = crate::parser::EditDiff {
            file_path: "x.rs".to_string(),
            old_string: "a\nb\n".to_string(),
            new_string: "a\nc\n".to_string(),
        };
        let m = make_match(Some(diff));
        let output = strip_ansi(&format_diff(&m, m.edit_diff.as_ref().unwrap(), &[], 200, 3));
        assert!(output.contains("@@"), "should contain @@ hunk marker");
    }

    #[test]
    fn test_format_diff_tool_name_in_header() {
        let diff = crate::parser::EditDiff {
            file_path: "y.rs".to_string(),
            old_string: "old".to_string(),
            new_string: "new".to_string(),
        };
        let m = make_match(Some(diff));
        let output = strip_ansi(&format_diff(&m, m.edit_diff.as_ref().unwrap(), &[], 200, 3));
        assert!(output.contains("tool: Edit"), "should show tool name");
    }

    #[test]
    fn test_format_diff_multiline() {
        let diff = crate::parser::EditDiff {
            file_path: "z.rs".to_string(),
            old_string: "line1\nline2\nline3".to_string(),
            new_string: "line1\nchanged\nline3".to_string(),
        };
        let m = make_match(Some(diff));
        let output = strip_ansi(&format_diff(&m, m.edit_diff.as_ref().unwrap(), &[], 200, 3));
        assert!(output.contains("-line2"), "should show removed line2");
        assert!(output.contains("+changed"), "should show added changed");
        // With proper unified diff: line1 and line3 are equal/context lines, not - or +
        assert!(!output.contains("-line1"), "line1 should not appear as removed (it's equal)");
        assert!(!output.contains("+line1"), "line1 should not appear as added (it's equal)");
    }

    #[test]
    fn test_color_tty_vs_piped() {
        let diff = crate::parser::EditDiff {
            file_path: "test.rs".to_string(),
            old_string: "old content".to_string(),
            new_string: "new content".to_string(),
        };
        let m = make_match(Some(diff));
        let pat = regex::Regex::new("old").unwrap();

        // Force colors on (simulates TTY)
        console::set_colors_enabled(true);
        let colored = format_diff(&m, m.edit_diff.as_ref().unwrap(), &[pat.clone()], 200, 3);
        assert!(colored.contains("\x1b["), "should have ANSI codes with colors enabled");

        // Force colors off (simulates piped output)
        console::set_colors_enabled(false);
        let plain = format_diff(&m, m.edit_diff.as_ref().unwrap(), &[pat], 200, 3);
        assert!(!plain.contains("\x1b["), "should not have ANSI codes with colors disabled");
    }

    #[test]
    fn test_format_diff_line_width_truncation() {
        let long_old = "x".repeat(300);
        let diff = crate::parser::EditDiff {
            file_path: "f.rs".to_string(),
            old_string: long_old,
            new_string: "short".to_string(),
        };
        let m = make_match(Some(diff));
        console::set_colors_enabled(false);
        let output = format_diff(&m, m.edit_diff.as_ref().unwrap(), &[], 50, 3);
        // The removed line: "-" prefix + truncated content (≤50 chars + "..." = 53) = ≤54 total
        let removed_line = output.lines().find(|l| l.starts_with('-') && !l.contains("---"))
            .expect("should have a removed line");
        let content = &removed_line[1..]; // strip '-' prefix
        assert!(content.len() <= 53,
            "removed line content should be truncated to ~50 chars, got {} chars: {:?}",
            content.len(), content);
    }

    #[test]
    fn test_format_diff_context_lines() {
        let old = "a\nb\nc\nd\ne\n";
        let new = "a\nb\nX\nd\ne\n";
        let diff = crate::parser::EditDiff {
            file_path: "f.rs".to_string(),
            old_string: old.to_string(),
            new_string: new.to_string(),
        };
        let m = make_match(Some(diff));
        console::set_colors_enabled(false);

        // With context_lines=0: only the changed lines, no context
        let output0 = format_diff(&m, m.edit_diff.as_ref().unwrap(), &[], 200, 0);
        assert!(!output0.lines().any(|l| l == " a"), "context=0 should not show 'a' as context");
        assert!(output0.contains("-c"), "should show removed 'c'");
        assert!(output0.contains("+X"), "should show added 'X'");

        // With context_lines=1: one context line on each side of the change
        let output1 = format_diff(&m, m.edit_diff.as_ref().unwrap(), &[], 200, 1);
        assert!(output1.lines().any(|l| l == " b"), "context=1 should show 'b' as context before");
        assert!(output1.lines().any(|l| l == " d"), "context=1 should show 'd' as context after");
        assert!(!output1.lines().any(|l| l == " a"), "context=1 should not show 'a' (too far)");
        assert!(!output1.lines().any(|l| l == " e"), "context=1 should not show 'e' (too far)");
    }

    #[test]
    fn test_format_diff_multi_hunk() {
        // Two changes far apart should produce two separate hunks
        let old = "a\nb\nc\nd\ne\nf\ng\nh\ni\nj\n";
        let new = "a\nB\nc\nd\ne\nf\ng\nh\ni\nJ\n";
        let diff = crate::parser::EditDiff {
            file_path: "f.rs".to_string(),
            old_string: old.to_string(),
            new_string: new.to_string(),
        };
        let m = make_match(Some(diff));
        console::set_colors_enabled(false);
        let output = format_diff(&m, m.edit_diff.as_ref().unwrap(), &[], 200, 1);

        // Should have two @@ hunk markers
        let hunk_count = output.lines().filter(|l| l.starts_with("@@")).count();
        assert_eq!(hunk_count, 2, "should have 2 hunks for changes 8 lines apart with context=1");
        assert!(output.contains("-b"), "should show removed 'b'");
        assert!(output.contains("+B"), "should show added 'B'");
        assert!(output.contains("-j"), "should show removed 'j'");
        assert!(output.contains("+J"), "should show added 'J'");
    }
}
