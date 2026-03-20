use crate::search::SearchMatch;
use regex::Regex;

const RESET: &str = "\x1b[0m";
const DIM: &str = "\x1b[2m";
const CYAN: &str = "\x1b[36m";
const BOLD_YELLOW: &str = "\x1b[1;33m";

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
