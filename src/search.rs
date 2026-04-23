use std::collections::HashSet;
use regex::Regex;

use crate::parser::{extract_content_opts, EditDiff, ExtractedContent, Target};
use crate::sessions::SessionFile;

pub struct MatchedLine {
    pub line: String,
    pub is_match: bool,
}

/// A record neighboring a match, at a given signed offset.
/// Negative offsets are records earlier in the session; positive are later.
pub struct ContextRecord {
    pub offset: i32,
    pub target: Target,
    pub tool_name: Option<String>,
    pub text: String,
    pub timestamp: String,
    pub edit_diff: Option<EditDiff>,
    pub raw_entry: Option<serde_json::Value>,
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
    /// Set for Edit tool-use matches when diff data is available.
    pub edit_diff: Option<EditDiff>,
    /// The original JSONL entry, preserved when `json_output` is set.
    pub raw_entry: Option<serde_json::Value>,
    /// Records before/after the match, sorted by signed offset.
    pub context_records: Vec<ContextRecord>,
}

#[derive(Clone)]
pub struct SearchOptions {
    pub patterns: Vec<Regex>,
    /// Targets that are candidates for *matching* the pattern.
    pub targets: HashSet<Target>,
    /// Targets to extract from session files. Must be a superset of `targets`.
    /// When record-level context is enabled this is usually the universe of
    /// known types so that offsets can walk over any record.
    pub extract_targets: HashSet<Target>,
    pub context_before: usize,
    pub context_after: usize,
    pub max_results: usize,
    #[allow(dead_code)]
    pub max_line_width: usize,
    #[allow(dead_code)]
    pub json_output: bool,
    #[allow(dead_code)]
    pub sessions_with_matches: bool,
    /// Render Edit tool matches as unified diffs.
    #[allow(dead_code)]
    pub diff_mode: bool,
    /// Sorted signed offsets of records to collect as context around each match.
    /// Does not include 0 (the match itself, which is always shown).
    pub context_offsets: Vec<i32>,
    /// When set, record-context offsets count (and display) only records of
    /// these types. Intermediate records of other types are hidden.
    pub context_type_filter: Option<HashSet<Target>>,
}

pub fn find_matches(
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
        line: lines[n].to_string(),
        is_match: matching_set.contains(&n),
    }).collect())
}

/// Gather records neighboring the match at `match_index` in `contents`.
///
/// When `type_filter` is `None`, offsets count over every record in `contents`.
/// When `Some`, offsets count only records whose target is in the filter; records
/// of other types are skipped entirely (they are not displayed as context either).
pub fn gather_context(
    contents: &[ExtractedContent],
    match_index: usize,
    offsets: &[i32],
    type_filter: Option<&HashSet<Target>>,
    keep_raw: bool,
) -> Vec<ContextRecord> {
    if offsets.is_empty() {
        return vec![];
    }

    let mut result: Vec<ContextRecord> = vec![];

    match type_filter {
        None => {
            for &off in offsets {
                let idx_opt = if off > 0 {
                    let target = match_index.checked_add(off as usize);
                    target.filter(|&i| i < contents.len())
                } else if off < 0 {
                    match_index.checked_sub((-off) as usize)
                } else {
                    None
                };
                if let Some(idx) = idx_opt {
                    result.push(make_context_record(&contents[idx], off, keep_raw));
                }
            }
        }
        Some(filter) => {
            let max_pos = *offsets.iter().max().unwrap_or(&0);
            let min_neg = *offsets.iter().min().unwrap_or(&0);
            let offset_set: HashSet<i32> = offsets.iter().copied().collect();

            if max_pos > 0 {
                let mut count: i32 = 0;
                let mut i = match_index + 1;
                while i < contents.len() && count < max_pos {
                    if filter.contains(&contents[i].target) {
                        count += 1;
                        if offset_set.contains(&count) {
                            result.push(make_context_record(&contents[i], count, keep_raw));
                        }
                    }
                    i += 1;
                }
            }

            if min_neg < 0 {
                let mut count: i32 = 0;
                let mut i = match_index;
                while i > 0 && count > min_neg {
                    i -= 1;
                    if filter.contains(&contents[i].target) {
                        count -= 1;
                        if offset_set.contains(&count) {
                            result.push(make_context_record(&contents[i], count, keep_raw));
                        }
                    }
                }
            }
        }
    }

    result.sort_by_key(|r| r.offset);
    result
}

fn make_context_record(r: &ExtractedContent, offset: i32, keep_raw: bool) -> ContextRecord {
    ContextRecord {
        offset,
        target: r.target.clone(),
        tool_name: r.tool_name.clone(),
        text: r.text.clone(),
        timestamp: r.timestamp.clone(),
        edit_diff: r.edit_diff.clone(),
        raw_entry: if keep_raw { r.raw_entry.clone() } else { None },
    }
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

        let contents = extract_content_opts(
            &session.file_path,
            &options.extract_targets,
            &session.session_id,
            session.is_subagent,
            options.json_output,
        );

        for (i, content) in contents.iter().enumerate() {
            if match_number >= options.max_results {
                break;
            }

            if !options.targets.contains(&content.target) {
                continue;
            }

            if let Some(matched_lines) = find_matches(
                &content.text,
                &options.patterns,
                options.context_before,
                options.context_after,
            ) {
                match_number += 1;
                let context_records = gather_context(
                    &contents,
                    i,
                    &options.context_offsets,
                    options.context_type_filter.as_ref(),
                    options.json_output,
                );
                on_match(SearchMatch {
                    match_number,
                    session_id: content.session_id.clone(),
                    timestamp: content.timestamp.clone(),
                    target: content.target.clone(),
                    tool_name: content.tool_name.clone(),
                    text: content.text.clone(),
                    matched_lines,
                    edit_diff: content.edit_diff.clone(),
                    raw_entry: content.raw_entry.clone(),
                    context_records,
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

    fn mk(target: Target, text: &str) -> ExtractedContent {
        ExtractedContent {
            target,
            text: text.to_string(),
            tool_name: None,
            timestamp: String::new(),
            session_id: "s".to_string(),
            edit_diff: None,
            raw_entry: None,
        }
    }

    #[test]
    fn test_gather_context_symmetric_window_no_filter() {
        let contents = vec![
            mk(Target::User, "u0"),
            mk(Target::Assistant, "a0"),
            mk(Target::User, "u1"),        // match here at index 2
            mk(Target::Assistant, "a1"),
            mk(Target::User, "u2"),
        ];
        let ctx = gather_context(&contents, 2, &[-2, -1, 1, 2], None, false);
        let texts: Vec<(i32, &str)> = ctx.iter().map(|c| (c.offset, c.text.as_str())).collect();
        assert_eq!(texts, vec![(-2, "u0"), (-1, "a0"), (1, "a1"), (2, "u2")]);
    }

    #[test]
    fn test_gather_context_clamps_at_start_of_session() {
        let contents = vec![
            mk(Target::User, "u0"),        // match at index 0
            mk(Target::Assistant, "a0"),
            mk(Target::User, "u1"),
        ];
        let ctx = gather_context(&contents, 0, &[-2, -1, 1, 2], None, false);
        // -2 and -1 are out of bounds; only +1 and +2 survive
        let offs: Vec<i32> = ctx.iter().map(|c| c.offset).collect();
        assert_eq!(offs, vec![1, 2]);
    }

    #[test]
    fn test_gather_context_clamps_at_end_of_session() {
        let contents = vec![
            mk(Target::User, "u0"),
            mk(Target::Assistant, "a0"),
            mk(Target::User, "u1"),        // match at last index
        ];
        let ctx = gather_context(&contents, 2, &[-1, 1, 2, 3], None, false);
        let offs: Vec<i32> = ctx.iter().map(|c| c.offset).collect();
        assert_eq!(offs, vec![-1]);
    }

    #[test]
    fn test_gather_context_type_filter_hides_intermediates() {
        // "next user prompt" should skip the assistant/bash-output records between.
        let contents = vec![
            mk(Target::User, "u0"),         // match at index 0
            mk(Target::Assistant, "a0"),
            mk(Target::BashOutput, "b0"),
            mk(Target::User, "u1"),         // this is the 1st user AFTER match
            mk(Target::Assistant, "a1"),
            mk(Target::User, "u2"),         // 2nd user AFTER match
        ];
        let filter: HashSet<Target> = [Target::User].into_iter().collect();
        let ctx = gather_context(&contents, 0, &[1, 2], Some(&filter), false);
        let texts: Vec<(i32, &str)> = ctx.iter().map(|c| (c.offset, c.text.as_str())).collect();
        assert_eq!(texts, vec![(1, "u1"), (2, "u2")]);
    }

    #[test]
    fn test_gather_context_type_filter_backward() {
        let contents = vec![
            mk(Target::User, "u0"),         // 2nd user BEFORE match
            mk(Target::Assistant, "a0"),
            mk(Target::User, "u1"),         // 1st user BEFORE match
            mk(Target::Assistant, "a1"),
            mk(Target::User, "match_here"), // match at index 4
        ];
        let filter: HashSet<Target> = [Target::User].into_iter().collect();
        let ctx = gather_context(&contents, 4, &[-2, -1], Some(&filter), false);
        let texts: Vec<(i32, &str)> = ctx.iter().map(|c| (c.offset, c.text.as_str())).collect();
        assert_eq!(texts, vec![(-2, "u0"), (-1, "u1")]);
    }

    #[test]
    fn test_gather_context_type_filter_insufficient_records() {
        // Asking for 3 user prompts after but only 1 exists.
        let contents = vec![
            mk(Target::User, "u0"),     // match
            mk(Target::Assistant, "a0"),
            mk(Target::User, "u1"),     // only 1 user after
            mk(Target::Assistant, "a1"),
        ];
        let filter: HashSet<Target> = [Target::User].into_iter().collect();
        let ctx = gather_context(&contents, 0, &[1, 2, 3], Some(&filter), false);
        let texts: Vec<(i32, &str)> = ctx.iter().map(|c| (c.offset, c.text.as_str())).collect();
        assert_eq!(texts, vec![(1, "u1")]);
    }

    #[test]
    fn test_gather_context_empty_offsets_returns_nothing() {
        let contents = vec![mk(Target::User, "u0")];
        let ctx = gather_context(&contents, 0, &[], None, false);
        assert!(ctx.is_empty());
    }

    #[test]
    fn test_gather_context_multi_type_filter() {
        // Filter on {User, Assistant} — should skip only BashOutput.
        let contents = vec![
            mk(Target::User, "u0"),       // match
            mk(Target::BashOutput, "b0"),
            mk(Target::Assistant, "a0"),  // 1st
            mk(Target::BashOutput, "b1"),
            mk(Target::User, "u1"),       // 2nd
        ];
        let filter: HashSet<Target> = [Target::User, Target::Assistant].into_iter().collect();
        let ctx = gather_context(&contents, 0, &[1, 2], Some(&filter), false);
        let texts: Vec<(i32, &str)> = ctx.iter().map(|c| (c.offset, c.text.as_str())).collect();
        assert_eq!(texts, vec![(1, "a0"), (2, "u1")]);
    }
}
