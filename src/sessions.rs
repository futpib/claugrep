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
