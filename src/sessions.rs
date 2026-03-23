use std::collections::{HashMap, HashSet};
use std::fs;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone)]
pub struct SessionFile {
    pub session_id: String,
    pub file_path: PathBuf,
    pub mtime: std::time::SystemTime,
    pub is_subagent: bool,
}

fn encode_project_path(path: &str) -> String {
    path.replace(['/', '.'], "-")
}

pub fn project_dir(project_path: &str) -> PathBuf {
    let home = dirs::home_dir().expect("no home dir");
    let encoded = encode_project_path(project_path);
    home.join(".claude").join("projects").join(encoded)
}

fn read_line2(path: &Path) -> Option<String> {
    let file = fs::File::open(path).ok()?;
    let mut reader = BufReader::new(file);
    let mut line = String::new();
    reader.read_line(&mut line).ok()?; // skip line 1
    line.clear();
    reader.read_line(&mut line).ok()?;
    Some(line.trim_end().to_string())
}

fn detect_chain_link(file_path: &Path, file_session_id: &str) -> (String, Option<String>) {
    let line2 = match read_line2(file_path) {
        Some(l) if !l.is_empty() => l,
        _ => return (file_session_id.to_string(), None),
    };

    let entry: serde_json::Value = match serde_json::from_str(&line2) {
        Ok(v) => v,
        Err(_) => return (file_session_id.to_string(), None),
    };

    if entry["type"] != "user" {
        return (file_session_id.to_string(), None);
    }

    let content = &entry["message"]["content"];
    let interrupt_text = "[Request interrupted by user for tool use]";

    let has_interrupt = if content.is_string() {
        content.as_str().unwrap_or("").contains(interrupt_text)
    } else if let Some(arr) = content.as_array() {
        arr.iter().any(|b| {
            b["type"] == "text" && b["text"].as_str().unwrap_or("").contains(interrupt_text)
        })
    } else {
        false
    };

    if has_interrupt {
        if let Some(linked) = entry["sessionId"].as_str() {
            if linked != file_session_id {
                return (file_session_id.to_string(), Some(linked.to_string()));
            }
        }
    }

    (file_session_id.to_string(), None)
}

fn find_subagent_files(project_dir: &Path, session_id: &str) -> Vec<SessionFile> {
    let subagent_dir = project_dir.join(session_id).join("subagents");
    let entries = match fs::read_dir(&subagent_dir) {
        Ok(e) => e,
        Err(_) => return vec![],
    };

    entries
        .flatten()
        .filter(|e| {
            let name = e.file_name();
            let s = name.to_string_lossy();
            s.starts_with("agent-") && s.ends_with(".jsonl")
        })
        .filter_map(|e| {
            let file_path = e.path();
            let mtime = fs::metadata(&file_path).ok()?.modified().ok()?;
            Some(SessionFile {
                session_id: session_id.to_string(),
                file_path,
                mtime,
                is_subagent: true,
            })
        })
        .collect()
}

/// Returns all git worktree paths for the given directory, or just the directory itself
/// if it is not a git repo or has no worktrees.
pub fn get_worktree_paths(cwd: &str) -> Vec<String> {
    use std::process::Command;
    let output = Command::new("git")
        .args(["worktree", "list", "--porcelain"])
        .current_dir(cwd)
        .output();
    match output {
        Ok(out) if out.status.success() => {
            let stdout = String::from_utf8_lossy(&out.stdout);
            let paths: Vec<String> = stdout
                .lines()
                .filter_map(|l| l.strip_prefix("worktree ").map(str::to_string))
                .collect();
            if paths.is_empty() { vec![cwd.to_string()] } else { paths }
        }
        _ => vec![cwd.to_string()],
    }
}

/// Information about a single project directory.
pub struct ProjectInfo {
    /// Raw directory name under ~/.claude/projects/ (e.g. `-home-user-code-proj`)
    pub encoded_path: String,
    /// Best-effort decoded path: replace `-` with `/` (lossy for paths with original `-`)
    pub decoded_path: String,
    /// Number of top-level `.jsonl` session files
    pub session_count: usize,
    /// Most recent session file mtime, if any
    pub latest_mtime: Option<std::time::SystemTime>,
}

/// List all project directories under ~/.claude/projects/.
pub fn discover_projects() -> Vec<ProjectInfo> {
    let home = dirs::home_dir().expect("no home dir");
    let projects_root = home.join(".claude").join("projects");

    let entries = match fs::read_dir(&projects_root) {
        Ok(e) => e,
        Err(_) => return vec![],
    };

    let mut projects: Vec<ProjectInfo> = entries
        .flatten()
        .filter(|e| e.file_type().map(|t| t.is_dir()).unwrap_or(false))
        .filter_map(|e| {
            let encoded = e.file_name().to_string_lossy().to_string();
            let dir = projects_root.join(&encoded);

            let mut session_count = 0usize;
            let mut latest_mtime: Option<std::time::SystemTime> = None;

            if let Ok(inner) = fs::read_dir(&dir) {
                for entry in inner.flatten() {
                    if entry.file_name().to_string_lossy().ends_with(".jsonl") {
                        session_count += 1;
                        if let Ok(meta) = fs::metadata(entry.path()) {
                            if let Ok(mtime) = meta.modified() {
                                latest_mtime = Some(match latest_mtime {
                                    None => mtime,
                                    Some(prev) => prev.max(mtime),
                                });
                            }
                        }
                    }
                }
            }

            let decoded = encoded.replace('-', "/");

            Some(ProjectInfo {
                encoded_path: encoded,
                decoded_path: decoded,
                session_count,
                latest_mtime,
            })
        })
        .collect();

    // Most-recently-active projects first
    projects.sort_by(|a, b| {
        let ta = a.latest_mtime.unwrap_or(std::time::UNIX_EPOCH);
        let tb = b.latest_mtime.unwrap_or(std::time::UNIX_EPOCH);
        tb.cmp(&ta)
    });

    projects
}

/// Discover sessions across ALL project directories under ~/.claude/projects/
pub fn discover_all_sessions() -> Vec<SessionFile> {
    let home = dirs::home_dir().expect("no home dir");
    let projects_root = home.join(".claude").join("projects");

    let entries = match fs::read_dir(&projects_root) {
        Ok(e) => e,
        Err(_) => return vec![],
    };

    let mut seen_paths = std::collections::HashSet::new();
    let mut all = vec![];

    for entry in entries.flatten() {
        if entry.file_type().map(|t| t.is_dir()).unwrap_or(false) {
            let encoded = entry.file_name().to_string_lossy().to_string();
            // Decode encoded project path (reverse of encode_project_path)
            // We don't need to decode — just discover all .jsonl in this dir
            let dir = projects_root.join(&encoded);
            let inner = match fs::read_dir(&dir) {
                Ok(e) => e,
                Err(_) => continue,
            };
            let jsonl_files: Vec<(String, PathBuf, std::time::SystemTime)> = inner
                .flatten()
                .filter(|e| e.file_name().to_string_lossy().ends_with(".jsonl"))
                .filter_map(|e| {
                    let path = e.path();
                    let mtime = fs::metadata(&path).ok()?.modified().ok()?;
                    let sid = e.file_name().to_string_lossy().replace(".jsonl", "").to_string();
                    Some((sid, path, mtime))
                })
                .collect();

            for (sid, path, mtime) in &jsonl_files {
                let path_str = path.to_string_lossy().to_string();
                if seen_paths.insert(path_str) {
                    all.push(SessionFile {
                        session_id: sid.clone(),
                        file_path: path.clone(),
                        mtime: *mtime,
                        is_subagent: false,
                    });
                    // Include subagents
                    for sf in find_subagent_files(&dir, sid) {
                        let sub_str = sf.file_path.to_string_lossy().to_string();
                        if seen_paths.insert(sub_str) {
                            all.push(sf);
                        }
                    }
                }
            }
        }
    }

    all
}

pub fn discover_sessions(project_path: &str, specific_session: Option<&str>) -> Vec<SessionFile> {
    let dir = project_dir(project_path);

    let entries = match fs::read_dir(&dir) {
        Ok(e) => e,
        Err(_) => return vec![],
    };

    let jsonl_files: Vec<(String, PathBuf, std::time::SystemTime)> = entries
        .flatten()
        .filter(|e| e.file_name().to_string_lossy().ends_with(".jsonl"))
        .filter_map(|e| {
            let path = e.path();
            let mtime = fs::metadata(&path).ok()?.modified().ok()?;
            let sid = e.file_name().to_string_lossy().replace(".jsonl", "").to_string();
            Some((sid, path, mtime))
        })
        .collect();

    if let Some(id) = specific_session {
        let matching: Vec<_> = jsonl_files
            .iter()
            .filter(|(sid, _, _)| sid.starts_with(id))
            .collect();

        let mut result = vec![];
        for (sid, path, mtime) in &matching {
            result.push(SessionFile {
                session_id: sid.clone(),
                file_path: path.clone(),
                mtime: *mtime,
                is_subagent: false,
            });
            result.extend(find_subagent_files(&dir, sid));
        }
        return result;
    }

    // Build chain links
    let chain_links: Vec<(String, Option<String>)> = jsonl_files
        .iter()
        .map(|(sid, path, _)| detect_chain_link(path, sid))
        .collect();

    let mut previous_to_next: HashMap<String, String> = HashMap::new();
    for (sid, prev) in &chain_links {
        if let Some(p) = prev {
            previous_to_next.insert(p.clone(), sid.clone());
        }
    }

    let mut previous_map: HashMap<String, String> = HashMap::new();
    for (sid, prev) in &chain_links {
        if let Some(p) = prev {
            previous_map.insert(sid.clone(), p.clone());
        }
    }

    let mtime_map: HashMap<String, std::time::SystemTime> = jsonl_files
        .iter()
        .map(|(sid, _, mtime)| (sid.clone(), *mtime))
        .collect();

    // Chain heads: sessions not pointed to by any other session
    let mut chain_heads: Vec<String> = chain_links
        .iter()
        .filter(|(sid, _)| !previous_to_next.contains_key(sid.as_str()))
        .map(|(sid, _)| sid.clone())
        .collect();

    // Sort heads newest first
    chain_heads.sort_by(|a, b| {
        let ta = mtime_map.get(a).cloned().unwrap_or(std::time::UNIX_EPOCH);
        let tb = mtime_map.get(b).cloned().unwrap_or(std::time::UNIX_EPOCH);
        tb.cmp(&ta)
    });

    // Walk chains
    let mut ordered: Vec<String> = vec![];
    let mut visited: HashSet<String> = HashSet::new();

    for head in &chain_heads {
        let mut current = Some(head.clone());
        let mut chain = vec![];
        while let Some(cur) = current {
            if visited.contains(&cur) {
                break;
            }
            chain.push(cur.clone());
            visited.insert(cur.clone());
            current = previous_map.get(&cur).cloned();
        }
        ordered.extend(chain);
    }

    // Remaining (unchained), sorted newest first
    let mut remaining: Vec<_> = jsonl_files
        .iter()
        .filter(|(sid, _, _)| !visited.contains(sid))
        .collect();
    remaining.sort_by(|a, b| b.2.cmp(&a.2));
    for (sid, _, _) in remaining {
        ordered.push(sid.clone());
    }

    let file_map: HashMap<String, (PathBuf, std::time::SystemTime)> = jsonl_files
        .into_iter()
        .map(|(sid, path, mtime)| (sid, (path, mtime)))
        .collect();

    let mut result = vec![];
    for sid in &ordered {
        if let Some((path, mtime)) = file_map.get(sid) {
            result.push(SessionFile {
                session_id: sid.clone(),
                file_path: path.clone(),
                mtime: *mtime,
                is_subagent: false,
            });
            result.extend(find_subagent_files(&dir, sid));
        }
    }

    result
}

/// Resolve a session selector: numeric offset, UUID prefix, or "all"
pub fn resolve_session(selector: Option<&str>, sessions: &[SessionFile]) -> Vec<SessionFile> {
    let selector = match selector {
        None | Some("all") => return sessions.to_vec(),
        Some(s) => s,
    };

    if let Ok(offset) = selector.parse::<i32>() {
        // Collect unique parent sessions in discovery order (newest-first)
        let mut parent_sessions: Vec<&SessionFile> = vec![];
        let mut seen: HashSet<&str> = HashSet::new();
        for s in sessions {
            if !s.is_subagent && !seen.contains(s.session_id.as_str()) {
                seen.insert(&s.session_id);
                parent_sessions.push(s);
            }
        }

        // Reverse to chronological (oldest-first) for indexing
        let chronological: Vec<_> = parent_sessions.iter().rev().collect();
        let total = chronological.len() as i32;

        let idx = if offset <= 0 {
            total - 1 + offset
        } else {
            offset - 1
        };

        if idx < 0 || idx >= total {
            eprintln!("Session offset {selector} out of range ({total} sessions available)");
            std::process::exit(1);
        }

        let target_id = &chronological[idx as usize].session_id;
        return sessions
            .iter()
            .filter(|s| &s.session_id == target_id)
            .cloned()
            .collect();
    }

    // UUID prefix match
    sessions
        .iter()
        .filter(|s| s.session_id.starts_with(selector))
        .cloned()
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::SystemTime;

    fn mock_session(id: &str) -> SessionFile {
        SessionFile {
            session_id: id.to_string(),
            file_path: std::path::PathBuf::from(format!("/tmp/{}.jsonl", id)),
            mtime: SystemTime::UNIX_EPOCH,
            is_subagent: false,
        }
    }

    #[test]
    fn test_encode_project_path() {
        assert_eq!(encode_project_path("/home/alice"), "-home-alice");
        assert_eq!(encode_project_path("/home/alice/code/my.project"), "-home-alice-code-my-project");
        assert_eq!(encode_project_path("/"), "-");
    }

    #[test]
    fn test_resolve_session_all() {
        let sessions = vec![mock_session("aaa"), mock_session("bbb")];
        let result = resolve_session(None, &sessions);
        assert_eq!(result.len(), 2);
        let result = resolve_session(Some("all"), &sessions);
        assert_eq!(result.len(), 2);
    }

    #[test]
    fn test_resolve_session_uuid_prefix() {
        let sessions = vec![
            mock_session("aabbccdd-0000-0000-0000-000000000000"),
            mock_session("bbccddee-0000-0000-0000-000000000000"),
        ];
        let result = resolve_session(Some("aabb"), &sessions);
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].session_id, "aabbccdd-0000-0000-0000-000000000000");
    }

    #[test]
    fn test_resolve_session_positive_offset() {
        let sessions = vec![
            mock_session("old-session"),
            mock_session("new-session"),
        ];
        // offset 1 = oldest (chronological first)
        let result = resolve_session(Some("1"), &sessions);
        assert_eq!(result.len(), 1);
        // offset 2 = newest
        let result2 = resolve_session(Some("2"), &sessions);
        assert_eq!(result2.len(), 1);
        assert_ne!(result[0].session_id, result2[0].session_id);
    }

    #[test]
    fn test_resolve_session_negative_offset() {
        let sessions = vec![
            mock_session("old-session"),
            mock_session("new-session"),
        ];
        // offset 0 = latest
        let result0 = resolve_session(Some("0"), &sessions);
        assert_eq!(result0.len(), 1);
        // offset -1 = second latest = same as 0 when there are only 2
        // (offset -1 means "one before latest", so index total-2)
        let result_neg1 = resolve_session(Some("-1"), &sessions);
        assert_eq!(result_neg1.len(), 1);
        assert_ne!(result0[0].session_id, result_neg1[0].session_id);
    }

    #[test]
    fn test_get_worktree_paths_non_git_dir() {
        let paths = get_worktree_paths("/tmp");
        assert_eq!(paths, vec!["/tmp".to_string()]);
    }

    #[test]
    fn test_get_worktree_paths_git_repo() {
        // When run from a git repo, should return at least one path
        let cwd = std::env::current_dir().unwrap();
        let paths = get_worktree_paths(&cwd.to_string_lossy());
        assert!(!paths.is_empty());
        for p in &paths {
            assert!(p.starts_with('/'), "expected absolute path, got: {}", p);
        }
    }
}
