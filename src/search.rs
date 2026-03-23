use std::collections::HashSet;
use regex::Regex;

use crate::parser::{build_tool_use_map, extract_content, Target};
use crate::sessions::SessionFile;

pub struct MatchedLine {
    pub line_number: usize,
    pub line: String,
    pub is_match: bool,
}

pub struct SearchMatch {
    pub match_number: usize,
    pub session_id: String,
    pub timestamp: String,
    pub target: Target,
    pub tool_name: Option<String>,
    #[allow(dead_code)]
    pub text: String,
    pub matched_lines: Vec<MatchedLine>,
}

pub struct SearchOptions {
    pub patterns: Vec<Regex>,
    pub targets: HashSet<String>,
    pub context_before: usize,
    pub context_after: usize,
    pub max_results: usize,
    #[allow(dead_code)]
    pub max_line_width: usize,
    #[allow(dead_code)]
    pub json_output: bool,
    #[allow(dead_code)]
    pub sessions_with_matches: bool,
}

fn find_matches(
    text: &str,
    patterns: &[Regex],
    context_before: usize,
    context_after: usize,
) -> Option<Vec<MatchedLine>> {
    let lines: Vec<&str> = text.split('\n').collect();
    let matching: Vec<usize> = lines.iter().enumerate()
        .filter(|(_, line)| patterns.iter().any(|p| p.is_match(line)))
        .map(|(i, _)| i)
        .collect();

    if matching.is_empty() {
        return None;
    }

    if context_before == 0 && context_after == 0 {
        return Some(matching.iter().map(|&n| MatchedLine {
            line_number: n,
            line: lines[n].to_string(),
            is_match: true,
        }).collect());
    }

    let mut visible: std::collections::BTreeSet<usize> = Default::default();
    for &n in &matching {
        let start = n.saturating_sub(context_before);
        let end = (n + context_after).min(lines.len() - 1);
        for i in start..=end {
            visible.insert(i);
        }
    }

    let matching_set: HashSet<usize> = matching.into_iter().collect();
    Some(visible.into_iter().map(|n| MatchedLine {
        line_number: n,
        line: lines[n].to_string(),
        is_match: matching_set.contains(&n),
    }).collect())
}

/// Search sessions, calling `on_match` for each result as it is found.
/// Returns the total number of matches.
pub fn search_sessions<F>(sessions: &[SessionFile], options: &SearchOptions, mut on_match: F) -> usize
where
    F: FnMut(SearchMatch),
{
    let mut match_number = 0;

    for session in sessions {
        if match_number >= options.max_results {
            break;
        }

        let tool_use_map = build_tool_use_map(&session.file_path);
        let contents = extract_content(
            &session.file_path,
            &tool_use_map,
            &options.targets,
            &session.session_id,
            session.is_subagent,
        );

        for content in contents {
            if match_number >= options.max_results {
                break;
            }

            if let Some(matched_lines) = find_matches(
                &content.text,
                &options.patterns,
                options.context_before,
                options.context_after,
            ) {
                match_number += 1;
                on_match(SearchMatch {
                    match_number,
                    session_id: content.session_id,
                    timestamp: content.timestamp,
                    target: content.target,
                    tool_name: content.tool_name,
                    text: content.text,
                    matched_lines,
                });
            }
        }
    }

    match_number
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pat(s: &str) -> Regex {
        Regex::new(s).unwrap()
    }

    #[test]
    fn test_find_matches_basic() {
        let result = find_matches("hello\nworld\nfoo", &[pat("world")], 0, 0);
        let lines = result.unwrap();
        assert_eq!(lines.len(), 1);
        assert_eq!(lines[0].line, "world");
        assert!(lines[0].is_match);
    }

    #[test]
    fn test_find_matches_no_match() {
        let result = find_matches("hello\nworld", &[pat("zzz")], 0, 0);
        assert!(result.is_none());
    }

    #[test]
    fn test_find_matches_with_context() {
        let text = "line1\nline2\nmatch\nline4\nline5";
        let result = find_matches(text, &[pat("match")], 1, 1).unwrap();
        assert_eq!(result.len(), 3);
        assert_eq!(result[0].line, "line2");
        assert!(!result[0].is_match);
        assert_eq!(result[1].line, "match");
        assert!(result[1].is_match);
        assert_eq!(result[2].line, "line4");
        assert!(!result[2].is_match);
    }

    #[test]
    fn test_find_matches_multiple_patterns() {
        let text = "apple\nbanana\ncherry";
        let result = find_matches(text, &[pat("apple"), pat("cherry")], 0, 0).unwrap();
        assert_eq!(result.len(), 2);
        assert!(result.iter().all(|l| l.is_match));
    }

    #[test]
    fn test_find_matches_context_clamps_to_bounds() {
        let text = "only\nmatch";
        let result = find_matches(text, &[pat("only")], 10, 10).unwrap();
        // context before line 0 is nothing, context after includes "match"
        assert_eq!(result.len(), 2);
    }
}
