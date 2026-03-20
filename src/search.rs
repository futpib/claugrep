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

pub fn search_sessions(sessions: &[SessionFile], options: &SearchOptions) -> Vec<SearchMatch> {
    let mut results = vec![];
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
                results.push(SearchMatch {
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

    results
}
