use crate::search::SearchMatch;
use crate::parser::ExtractedContent;
use regex::Regex;

const RESET: &str = "\x1b[0m";
const DIM: &str = "\x1b[2m";
const CYAN: &str = "\x1b[36m";
const BOLD_YELLOW: &str = "\x1b[1;33m";
const RED: &str = "\x1b[31m";
const GREEN: &str = "\x1b[32m";

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

        let start = match_start.saturating_sub(before);
        let end = (match_start + match_len + after).min(line.len());

        let prefix = if start > 0 { "..." } else { "" };
        let suffix = if end < line.len() { "..." } else { "" };
        (format!("{}{}{}", prefix, &line[start..end], suffix), true)
    } else {
        (format!("{}...", &line[..max_width]), true)
    }
}

fn highlight_matches(line: &str, patterns: &[Regex], max_width: usize) -> String {
    let (truncated, _) = truncate_line(line, patterns, max_width);
    let mut result = truncated;
    for p in patterns {
        result = p.replace_all(&result, |caps: &regex::Captures| {
            format!("{}{}{}", BOLD_YELLOW, &caps[0], RESET)
        }).to_string();
    }
    result
}

/// Render an Edit tool call as a unified diff block.
pub fn format_diff(m: &SearchMatch, diff: &crate::parser::EditDiff, patterns: &[Regex]) -> String {
    let short_session = &m.session_id[..m.session_id.len().min(8)];
    let time = format_timestamp(&m.timestamp);
    let header = format!("{}--- Match #{} | session={} | {} | tool-use ---{}",
        CYAN, m.match_number, short_session, time, RESET);

    let display_path = diff.file_path.trim_start_matches('/');
    let mut lines = vec![header];
    lines.push(format!("{}tool: Edit{}", DIM, RESET));
    lines.push(format!("{}--- a/{}{}", RED, display_path, RESET));
    lines.push(format!("{}+++ b/{}{}", GREEN, display_path, RESET));

    let old_lines: Vec<&str> = diff.old_string.split('\n').collect();
    let new_lines: Vec<&str> = diff.new_string.split('\n').collect();

    lines.push(format!("{}@@ -{},{} +{},{} @@{}",
        DIM,
        1, old_lines.len(),
        1, new_lines.len(),
        RESET));

    for line in &old_lines {
        let highlighted = highlight_matches_colored(line, patterns, RED);
        lines.push(format!("{}-{}{}", RED, highlighted, RESET));
    }
    for line in &new_lines {
        let highlighted = highlight_matches_colored(line, patterns, GREEN);
        lines.push(format!("{}+{}{}", GREEN, highlighted, RESET));
    }

    lines.join("\n")
}

/// Like `highlight_matches` but uses `base_color` instead of bold-yellow so
/// the match highlight stays visible against colored diff lines.
fn highlight_matches_colored(line: &str, patterns: &[Regex], base_color: &str) -> String {
    let mut result = line.to_string();
    for p in patterns {
        result = p.replace_all(&result, |caps: &regex::Captures| {
            format!("{}{}{}{}", BOLD_YELLOW, &caps[0], RESET, base_color)
        }).to_string();
    }
    result
}

pub fn format_match(m: &SearchMatch, patterns: &[Regex], max_width: usize) -> String {
    let short_session = &m.session_id[..m.session_id.len().min(8)];
    let time = format_timestamp(&m.timestamp);
    let header = format!("{}--- Match #{} | session={} | {} | {} ---{}",
        CYAN, m.match_number, short_session, time, m.target.as_str(), RESET);

    let mut lines = vec![header];

    if let Some(ref tool) = m.tool_name {
        lines.push(format!("{}tool: {}{}", DIM, tool, RESET));
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
    let header = format!("{}--- session={} | {} | {}{} ---{}",
        CYAN, short_session, time, target, tool_suffix, RESET);

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
            format!("{} {}[+{} more lines]{}", first, DIM, remaining, RESET)
        } else {
            first.to_string()
        }
    } else {
        text
    };

    format!("{}\n{}", header, display)
}

pub fn format_summary(count: usize, project_path: &str, session_count: usize) -> String {
    let project_info = format!("{}Searched {} session{} for project {}{}",
        DIM,
        session_count,
        if session_count == 1 { "" } else { "s" },
        project_path,
        RESET);

    if count == 0 {
        format!("{}\nNo matches found.", project_info)
    } else {
        format!("\n{}\n{}{} match{} found.{}",
            project_info, DIM, count, if count == 1 { "" } else { "es" }, RESET)
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
        let output = strip_ansi(&format_diff(&m, m.edit_diff.as_ref().unwrap(), &[pat]));
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
        let output = strip_ansi(&format_diff(&m, m.edit_diff.as_ref().unwrap(), &[]));
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
        let output = strip_ansi(&format_diff(&m, m.edit_diff.as_ref().unwrap(), &[]));
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
        let output = strip_ansi(&format_diff(&m, m.edit_diff.as_ref().unwrap(), &[]));
        assert!(output.contains("-line2"), "should show removed line2");
        assert!(output.contains("+changed"), "should show added changed");
        // line1 and line3 appear in both old and new (still shown as - and + since we do full replacement)
        assert!(output.contains("-line1"), "should show old line1");
        assert!(output.contains("+line1"), "should show new line1");
    }
}
