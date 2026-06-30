/// Integration tests against real Claude session transcripts in ~/.claude/projects/.
///
/// These tests run the `claugrep` binary and verify end-to-end behaviour.
/// They require actual Claude Code transcript data to be present; in clean
/// environments without transcripts the tests skip gracefully.

use std::path::PathBuf;
use std::process::Command;

fn claugrep_claude() -> Command {
    let mut c = Command::new(env!("CARGO_BIN_EXE_claugrep"));
    c.args(["--backend", "claude"]);
    c
}

fn home_dir() -> PathBuf {
    dirs::home_dir().expect("no home dir")
}

fn home_project() -> String {
    home_dir().to_string_lossy().to_string()
}

/// Return true if `~/.claude/projects/<encoded>` exists for the given project.
fn project_has_sessions(project: &str) -> bool {
    let encoded = project.replace(['/', '.'], "-");
    let dir = home_dir()
        .join(".claude")
        .join("projects")
        .join(&encoded);
    dir.exists()
}

// ── --before / --until date filter ────────────────────────────────────────────

#[test]
fn test_before_far_future_shows_all_sessions() {
    if !project_has_sessions(&home_project()) {
        eprintln!("SKIP: no Claude sessions found");
        return;
    }
    // All sessions should pre-date year 2099 — result should be non-empty.
    let out = claugrep_claude()
        .args(["--before", "2099-01-01", "sessions", "--project", &home_project()])
        .output()
        .expect("failed to run claugrep");

    assert!(out.status.success(), "stderr: {}", String::from_utf8_lossy(&out.stderr));
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(!stdout.trim().is_empty(), "--before far future should still show sessions");
}

#[test]
fn test_before_ancient_past_exits_nonzero() {
    if !project_has_sessions(&home_project()) {
        eprintln!("SKIP: no Claude sessions found");
        return;
    }
    // No sessions exist from before 1971 — should exit nonzero.
    let out = claugrep_claude()
        .args(["--before", "1971-01-01", "sessions", "--project", &home_project()])
        .output()
        .expect("failed to run claugrep");

    assert!(!out.status.success(), "--before ancient past should exit nonzero (no sessions)");
}

#[test]
fn test_until_alias_matches_before() {
    if !project_has_sessions(&home_project()) {
        eprintln!("SKIP: no Claude sessions found");
        return;
    }
    let out_before = claugrep_claude()
        .args(["--before", "2099-01-01", "sessions", "--project", &home_project()])
        .output()
        .expect("failed to run claugrep");
    let out_until = claugrep_claude()
        .args(["--until", "2099-01-01", "sessions", "--project", &home_project()])
        .output()
        .expect("failed to run claugrep");

    assert!(out_before.status.success());
    assert!(out_until.status.success());
    assert_eq!(
        String::from_utf8_lossy(&out_before.stdout),
        String::from_utf8_lossy(&out_until.stdout),
        "--until should produce identical output to --before"
    );
}

#[test]
fn test_before_search_far_future_finds_results() {
    if !project_has_sessions(&home_project()) {
        eprintln!("SKIP: no Claude sessions found");
        return;
    }
    // With --before far-future, search should behave normally.
    let out = claugrep_claude()
        .args(["--before", "2099-01-01", "search", "claugrep", "-t", "user", "--project", &home_project()])
        .output()
        .expect("failed to run claugrep");

    assert!(out.status.success());
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("claugrep"), "--before 2099 should not filter out real sessions");
}

#[test]
fn test_after_and_before_combined_wide_window() {
    if !project_has_sessions(&home_project()) {
        eprintln!("SKIP: no Claude sessions found");
        return;
    }
    // A window from 1970 to 2099 should include all sessions.
    let out = claugrep_claude()
        .args(["--after", "1970-01-01", "--before", "2099-01-01",
               "sessions", "--project", &home_project()])
        .output()
        .expect("failed to run claugrep");

    assert!(out.status.success(), "stderr: {}", String::from_utf8_lossy(&out.stderr));
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(!stdout.trim().is_empty(), "wide window should include sessions");
}

#[test]
fn test_before_invalid_date_exits_nonzero() {
    let out = claugrep_claude()
        .args(["--before", "not-a-valid-date-xyz", "sessions", "--project", &home_project()])
        .output()
        .expect("failed to run claugrep");

    assert!(!out.status.success(), "invalid --before value should exit nonzero");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("cannot parse date") || stderr.contains("error"));
}

// ── sessions command ──────────────────────────────────────────────────────────

#[test]
fn test_sessions_lists_home_project() {
    if !project_has_sessions(&home_project()) {
        eprintln!("SKIP: no Claude sessions found");
        return;
    }
    let out = claugrep_claude()
        .args(["sessions", "--project", &home_project()])
        .output()
        .expect("failed to run claugrep");

    assert!(out.status.success(), "stderr: {}", String::from_utf8_lossy(&out.stderr));
    let stdout = String::from_utf8_lossy(&out.stdout);
    // Each line is: "YYYY-MM-DD HH:MM:SS <uuid>"
    assert!(!stdout.trim().is_empty(), "expected at least one session");
    let first_line = stdout.lines().next().unwrap();
    assert!(first_line.len() > 20, "expected timestamp + session ID");
}

#[test]
fn test_sessions_json_output() {
    if !project_has_sessions(&home_project()) {
        eprintln!("SKIP: no Claude sessions found");
        return;
    }
    let out = claugrep_claude()
        .args(["sessions", "--project", &home_project(), "--json"])
        .output()
        .expect("failed to run claugrep");

    assert!(out.status.success());
    let stdout = String::from_utf8_lossy(&out.stdout);
    let parsed: serde_json::Value = serde_json::from_str(&stdout)
        .expect("sessions --json should produce valid JSON");
    let arr = parsed.as_array().expect("expected JSON array");
    assert!(!arr.is_empty());
    // Each element should have sessionId, filePath, mtime, isSubagent
    let first = &arr[0];
    assert!(first["sessionId"].is_string());
    assert!(first["filePath"].is_string());
    assert!(first["mtime"].is_number());
    assert!(first["isSubagent"].is_boolean());
}

#[test]
fn test_sessions_missing_project_exits_nonzero() {
    let out = claugrep_claude()
        .args(["sessions", "--project", "/nonexistent/path/xyz"])
        .output()
        .expect("failed to run claugrep");
    assert!(!out.status.success());
}

// ── search command ────────────────────────────────────────────────────────────

#[test]
fn test_search_finds_known_user_message() {
    if !project_has_sessions(&home_project()) {
        eprintln!("SKIP: no Claude sessions found");
        return;
    }
    // "claugrep" appears in at least one user message (this very task was requested)
    let out = claugrep_claude()
        .args(["search", "claugrep", "-t", "user", "--project", &home_project()])
        .output()
        .expect("failed to run claugrep");

    assert!(out.status.success());
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("claugrep"), "expected match in stdout");
    assert!(stdout.contains("match"), "expected summary line");
}

#[test]
fn test_search_no_matches_exits_zero_with_message() {
    if !project_has_sessions(&home_project()) {
        eprintln!("SKIP: no Claude sessions found");
        return;
    }
    let out = claugrep_claude()
        .args(["search", "ZZZNOTFOUNDSTRING9876543210", "--project", &home_project()])
        .output()
        .expect("failed to run claugrep");

    assert!(out.status.success());
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("No matches found"));
}

#[test]
fn test_search_json_output_structure() {
    if !project_has_sessions(&home_project()) {
        eprintln!("SKIP: no Claude sessions found");
        return;
    }
    let out = claugrep_claude()
        .args(["search", "claugrep", "-t", "user", "--json", "--project", &home_project()])
        .output()
        .expect("failed to run claugrep");

    assert!(out.status.success());
    let stdout = String::from_utf8_lossy(&out.stdout);
    let lines: Vec<&str> = stdout.lines().collect();
    assert!(!lines.is_empty(), "should have at least one JSONL line");
    let first: serde_json::Value = serde_json::from_str(lines[0])
        .expect("each line must be valid JSON");
    assert!(first["type"].is_string(), "raw entry should have a type field");
    assert_eq!(first["type"].as_str(), Some("user"));
    assert!(first["sessionId"].is_string());
}

#[test]
fn test_search_case_insensitive() {
    if !project_has_sessions(&home_project()) {
        eprintln!("SKIP: no Claude sessions found");
        return;
    }
    let _out_sensitive = claugrep_claude()
        .args(["search", "CLAUGREP", "-t", "user", "--project", &home_project()])
        .output()
        .expect("failed to run claugrep");
    let out_insensitive = claugrep_claude()
        .args(["search", "CLAUGREP", "-t", "user", "--ignore-case", "--project", &home_project()])
        .output()
        .expect("failed to run claugrep");

    let stdout_insensitive = String::from_utf8_lossy(&out_insensitive.stdout);
    // Case-insensitive should find at least what case-sensitive does, and typically more
    // At minimum, check case-insensitive doesn't crash and finds results
    assert!(out_insensitive.status.success());
    assert!(!stdout_insensitive.contains("No matches found"),
        "case-insensitive search should find 'claugrep' even when querying 'CLAUGREP'");
}

#[test]
fn test_search_sessions_with_matches_flag() {
    if !project_has_sessions(&home_project()) {
        eprintln!("SKIP: no Claude sessions found");
        return;
    }
    let out = claugrep_claude()
        .args(["search", "claugrep", "-t", "user", "-l", "--project", &home_project()])
        .output()
        .expect("failed to run claugrep");

    assert!(out.status.success());
    let stdout = String::from_utf8_lossy(&out.stdout);
    // Each line should be a file path ending in .jsonl
    for line in stdout.lines() {
        assert!(line.ends_with(".jsonl"), "expected .jsonl path, got: {}", line);
        assert!(line.contains(".claude/projects/"), "expected path in .claude/projects/");
    }
}

#[test]
fn test_search_sessions_with_matches_no_results_exits_nonzero() {
    if !project_has_sessions(&home_project()) {
        eprintln!("SKIP: no Claude sessions found");
        return;
    }
    let out = claugrep_claude()
        .args(["search", "ZZZNOTFOUND9876543210", "-l", "--project", &home_project()])
        .output()
        .expect("failed to run claugrep");
    assert!(!out.status.success(), "should exit nonzero when no sessions match");
}

#[test]
fn test_search_context_lines() {
    if !project_has_sessions(&home_project()) {
        eprintln!("SKIP: no Claude sessions found");
        return;
    }
    let out_no_ctx = claugrep_claude()
        .args(["search", "claugrep", "-t", "user", "--max-results", "1", "--project", &home_project()])
        .output()
        .expect("failed to run claugrep");
    let out_with_ctx = claugrep_claude()
        .args(["search", "claugrep", "-t", "user", "-C", "2", "--max-results", "1", "--project", &home_project()])
        .output()
        .expect("failed to run claugrep");

    // With context lines, output should be same or longer
    let no_ctx_lines = String::from_utf8_lossy(&out_no_ctx.stdout).lines().count();
    let ctx_lines = String::from_utf8_lossy(&out_with_ctx.stdout).lines().count();
    assert!(ctx_lines >= no_ctx_lines, "context should produce >= lines");
}

#[test]
fn test_search_regex_pattern() {
    if !project_has_sessions(&home_project()) {
        eprintln!("SKIP: no Claude sessions found");
        return;
    }
    // Regex alternation: match "claugrep" or "claudex"
    let out = claugrep_claude()
        .args(["search", "clau(grep|dex)", "-t", "user", "--project", &home_project()])
        .output()
        .expect("failed to run claugrep");

    assert!(out.status.success());
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(!stdout.contains("No matches found"), "regex should find at least one match");
}

// ── dump command ──────────────────────────────────────────────────────────────

#[test]
fn test_dump_first_session() {
    if !project_has_sessions(&home_project()) {
        eprintln!("SKIP: no Claude sessions found");
        return;
    }
    let out = claugrep_claude()
        .args(["dump", "1", "--project", &home_project()])
        .output()
        .expect("failed to run claugrep");

    assert!(out.status.success(), "stderr: {}", String::from_utf8_lossy(&out.stderr));
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(!stdout.trim().is_empty(), "dump should produce output");
}

#[test]
fn test_dump_negative_offset() {
    if !project_has_sessions(&home_project()) {
        eprintln!("SKIP: no Claude sessions found");
        return;
    }
    // -1 means "second latest session" — should work without needing `-- -1`
    let out = claugrep_claude()
        .args(["dump", "-1", "--project", &home_project()])
        .output()
        .expect("failed to run claugrep");

    assert!(out.status.success(), "stderr: {}", String::from_utf8_lossy(&out.stderr));
}

#[test]
fn test_dump_targets_filter() {
    if !project_has_sessions(&home_project()) {
        eprintln!("SKIP: no Claude sessions found");
        return;
    }
    let out = claugrep_claude()
        .args(["dump", "1", "--targets", "assistant", "--project", &home_project()])
        .output()
        .expect("failed to run claugrep");

    assert!(out.status.success());
    let stdout = String::from_utf8_lossy(&out.stdout);
    // Dump prints a `[<target>]` prefix once per record and then the message body
    // (which may span many unprefixed lines). Only prefix-bearing lines should
    // identify the target, and they must all be `[assistant]`.
    let prefix = regex::Regex::new(r"^\[[a-z-]+\]").unwrap();
    let mut saw_assistant_prefix = false;
    for line in stdout.lines() {
        if prefix.is_match(line) {
            assert!(line.starts_with("[assistant]"),
                "non-assistant prefix leaked through --targets assistant: {}", line);
            saw_assistant_prefix = true;
        }
    }
    assert!(saw_assistant_prefix, "expected at least one [assistant] block in dump output");
}

#[test]
fn test_dump_missing_session_exits_nonzero() {
    if !project_has_sessions(&home_project()) {
        eprintln!("SKIP: no Claude sessions found");
        return;
    }
    let out = claugrep_claude()
        .args(["dump", "nonexistent-session-id", "--project", &home_project()])
        .output()
        .expect("failed to run claugrep");
    assert!(!out.status.success());
}

