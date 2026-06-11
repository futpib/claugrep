use std::collections::HashSet;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

/// Backend identifier stored on every `SessionFile` / `ProjectInfo` so the
/// `MultiSource` composite (and output formatters) can route/dispatch/annotate
/// records back to the source that produced them. This is a `&'static str`
/// rather than an enum so new backends (codex, …) can be added without editing
/// a shared enum — the `Source::name()` impl is the single source of truth.
pub const CLAUDE: &str = "claude";

/// Read a directory, returning `None` silently for `NotFound` (e.g. cleaned-up worktrees)
/// and logging a warning for other errors.
fn try_read_dir(path: &Path) -> Option<fs::ReadDir> {
    match fs::read_dir(path) {
        Ok(entries) => Some(entries),
        Err(e) if e.kind() == io::ErrorKind::NotFound => None,
        Err(e) => {
            eprintln!("warning: failed to read directory {}: {}", path.display(), e);
            None
        }
    }
}

#[derive(Debug, Clone)]
pub struct SessionFile {
    pub session_id: String,
    pub file_path: PathBuf,
    pub mtime: std::time::SystemTime,
    pub is_subagent: bool,
    /// Which `Source` produced this session (e.g. `claude`, `opencode`).
    pub backend: &'static str,
}

fn encode_project_path(path: &str) -> String {
    path.replace(['/', '.'], "-")
}

/// Return the default Claude config dir. Checks CLAUDE_CONFIG_DIR env var first, then ~/.claude.
pub fn default_claude_config_dir() -> PathBuf {
    if let Ok(dir) = std::env::var("CLAUDE_CONFIG_DIR") {
        return PathBuf::from(dir);
    }
    dirs::home_dir().expect("no home dir").join(".claude")
}

fn claudex_accounts_root() -> PathBuf {
    dirs::config_dir().expect("no config dir").join("claudex").join("accounts")
}

pub fn claudex_account_config_dir(account: &str) -> PathBuf {
    claudex_accounts_root().join(account).join("claude")
}

pub fn list_claudex_accounts() -> Vec<String> {
    let root = claudex_accounts_root();
    match fs::read_dir(&root) {
        Err(_) => vec![],
        Ok(entries) => entries.flatten()
            .filter(|e| e.file_type().map(|t| t.is_dir()).unwrap_or(false))
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect(),
    }
}

pub fn project_dir(project_path: &str, config_dir: &Path) -> PathBuf {
    let encoded = encode_project_path(project_path);
    config_dir.join("projects").join(encoded)
}

fn find_subagent_files(project_dir: &Path, session_id: &str) -> Vec<SessionFile> {
    let subagent_dir = project_dir.join(session_id).join("subagents");
    let entries = match try_read_dir(&subagent_dir) {
        Some(e) => e,
        None => return vec![],
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
            let meta = match fs::metadata(&file_path) {
                Ok(m) => m,
                Err(err) => {
                    eprintln!("warning: failed to read metadata for {}: {}", file_path.display(), err);
                    return None;
                }
            };
            let mtime = match meta.modified() {
                Ok(t) => t,
                Err(err) => {
                    eprintln!("warning: failed to get modification time for {}: {}", file_path.display(), err);
                    return None;
                }
            };
            Some(SessionFile {
                session_id: session_id.to_string(),
                file_path,
                mtime,
                is_subagent: true,
                backend: CLAUDE,
            })
        })
        .collect()
}

/// Returns all git worktree paths for the given directory, or just the directory itself
/// if it is not a git repo or has no worktrees.
pub fn get_worktree_paths(cwd: &str) -> Vec<String> {
    use std::process::Command;
    // Only expand worktrees when `cwd` is itself the root of a git worktree.
    // Running git from a *subdirectory* of a repo resolves the enclosing repo,
    // so `worktree list` would return an ancestor's worktree roots — paths
    // unrelated to `cwd`. Unioning their sessions would wrongly pull a parent
    // project's history into this one (e.g. `/home/claude/code` inheriting all
    // of `/home/claude`'s sessions). Guard by requiring the repo toplevel to be
    // `cwd` itself before trusting `worktree list`.
    let toplevel = Command::new("git")
        .args(["rev-parse", "--show-toplevel"])
        .current_dir(cwd)
        .output();
    match toplevel {
        Ok(out) if out.status.success() => {
            let top = String::from_utf8_lossy(&out.stdout).trim().to_string();
            let same = match (std::fs::canonicalize(cwd), std::fs::canonicalize(&top)) {
                (Ok(a), Ok(b)) => a == b,
                _ => top == cwd,
            };
            if !same {
                return vec![cwd.to_string()];
            }
        }
        // Not a git repo (or git unavailable): treat as a lone directory.
        _ => return vec![cwd.to_string()],
    }
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

/// Discover sessions for a project path, including all git worktrees, deduplicating by file path.
pub fn discover_sessions_with_worktrees(project_path: &str, config_dir: &Path) -> Vec<SessionFile> {
    let worktree_paths = get_worktree_paths(project_path);
    let mut unique_paths: Vec<String> = worktree_paths;
    if !unique_paths.contains(&project_path.to_string()) {
        unique_paths.push(project_path.to_string());
    }
    let mut seen_paths = std::collections::HashSet::new();
    unique_paths.iter()
        .flat_map(|p| discover_sessions(p, None, config_dir))
        .filter(|s| seen_paths.insert(s.file_path.to_string_lossy().to_string()))
        .collect()
}

/// Information about a single project directory.
pub struct ProjectInfo {
    pub encoded_path: String,
    pub decoded_path: String,
    pub verified: bool,
    pub session_count: usize,
    pub latest_mtime: Option<std::time::SystemTime>,
    pub account: Option<String>,
    pub backend: &'static str,
}

fn try_verify_decoded_path(encoded: &str) -> (String, bool) {
    let naive = encoded.replace('-', "/");
    if Path::new(&naive).exists() {
        return (naive, true);
    }
    let trimmed = encoded.trim_start_matches('-');
    let tokens: Vec<&str> = trimmed.split('-').collect();
    if !tokens.is_empty() {
        if let Some(found) = walk_and_verify(Path::new("/"), &tokens) {
            return (found, true);
        }
    }
    (encoded.to_string(), false)
}

fn walk_and_verify(dir: &Path, tokens: &[&str]) -> Option<String> {
    if tokens.is_empty() {
        return Some(dir.to_string_lossy().into_owned());
    }
    for take in 1..=tokens.len() {
        let name_hyphen = tokens[..take].join("-");
        let child = dir.join(&name_hyphen);
        if child.exists() {
            if let Some(result) = walk_and_verify(&child, &tokens[take..]) {
                return Some(result);
            }
        }
        if take > 1 {
            let name_dot = tokens[..take].join(".");
            if name_dot != name_hyphen {
                let child_dot = dir.join(&name_dot);
                if child_dot.exists() {
                    if let Some(result) = walk_and_verify(&child_dot, &tokens[take..]) {
                        return Some(result);
                    }
                }
            }
        }
    }
    None
}

/// List all project directories under the given config dirs.
pub fn discover_projects(config_dirs: &[(Option<String>, PathBuf)]) -> Vec<ProjectInfo> {
    let mut all_projects: Vec<ProjectInfo> = vec![];

    for (account, config_dir) in config_dirs {
        let projects_root = config_dir.join("projects");

        let entries = match try_read_dir(&projects_root) {
            Some(e) => e,
            None => continue,
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

                let (decoded, verified) = try_verify_decoded_path(&encoded);

                Some(ProjectInfo {
                    encoded_path: encoded,
                    decoded_path: decoded,
                    verified,
                    session_count,
                    latest_mtime,
                    account: account.clone(),
                    backend: CLAUDE,
                })
            })
            .collect();

        all_projects.append(&mut projects);
    }

    // Most-recently-active projects first
    all_projects.sort_by(|a, b| {
        let ta = a.latest_mtime.unwrap_or(std::time::UNIX_EPOCH);
        let tb = b.latest_mtime.unwrap_or(std::time::UNIX_EPOCH);
        tb.cmp(&ta)
    });

    all_projects
}

pub fn discover_sessions(project_path: &str, specific_session: Option<&str>, config_dir: &Path) -> Vec<SessionFile> {
    let dir = project_dir(project_path, config_dir);

    let entries = match try_read_dir(&dir) {
        Some(e) => e,
        None => return vec![],
    };

    let jsonl_files: Vec<(String, PathBuf, std::time::SystemTime)> = entries
        .flatten()
        .filter(|e| e.file_name().to_string_lossy().ends_with(".jsonl"))
        .filter_map(|e| {
            let path = e.path();
            let meta = match fs::metadata(&path) {
                Ok(m) => m,
                Err(err) => {
                    eprintln!("warning: failed to read metadata for {}: {}", path.display(), err);
                    return None;
                }
            };
            let mtime = match meta.modified() {
                Ok(t) => t,
                Err(err) => {
                    eprintln!("warning: failed to get modification time for {}: {}", path.display(), err);
                    return None;
                }
            };
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
                backend: CLAUDE,
            });
            result.extend(find_subagent_files(&dir, sid));
        }
        return result;
    }

    // Sort newest first
    let mut sorted = jsonl_files;
    sorted.sort_by(|a, b| b.2.cmp(&a.2));

    let mut result = vec![];
    for (sid, path, mtime) in &sorted {
        result.push(SessionFile {
            session_id: sid.clone(),
            file_path: path.clone(),
            mtime: *mtime,
            is_subagent: false,
            backend: CLAUDE,
        });
        result.extend(find_subagent_files(&dir, sid));
    }

    result
}

/// The session's display title: the most recent user-set `custom-title` if the
/// session has one, otherwise the most recent `ai-title`. Returns `None` when
/// the session has neither. Title records are rewritten frequently, so the last
/// occurrence wins.
pub fn session_title(file_path: &Path) -> Option<String> {
    use std::io::BufRead;
    let file = fs::File::open(file_path).ok()?;
    let mut custom: Option<String> = None;
    let mut ai: Option<String> = None;
    for line in io::BufReader::new(file).lines() {
        let line = match line {
            Ok(l) => l,
            Err(_) => continue,
        };
        // Cheap pre-filter: skip lines that can't be a title record.
        if !line.contains("\"custom-title\"") && !line.contains("\"ai-title\"") {
            continue;
        }
        let entry: serde_json::Value = match serde_json::from_str(&line) {
            Ok(e) => e,
            Err(_) => continue,
        };
        match entry["type"].as_str() {
            Some("custom-title") => {
                let t = entry["customTitle"].as_str().unwrap_or("");
                if !t.is_empty() {
                    custom = Some(t.to_string());
                }
            }
            Some("ai-title") => {
                let t = entry["aiTitle"].as_str().unwrap_or("");
                if !t.is_empty() {
                    ai = Some(t.to_string());
                }
            }
            _ => {}
        }
    }
    custom.or(ai)
}

/// Resolve a session selector: numeric offset, UUID prefix, or "all"
pub fn resolve_session(selector: Option<&str>, sessions: &[SessionFile]) -> Result<Vec<SessionFile>, String> {
    let selector = match selector {
        None | Some("all") => return Ok(sessions.to_vec()),
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
            return Err(format!("Session offset {selector} out of range ({total} sessions available)"));
        }

        let target_id = &chronological[idx as usize].session_id;
        return Ok(sessions
            .iter()
            .filter(|s| &s.session_id == target_id)
            .cloned()
            .collect());
    }

    // UUID prefix match
    Ok(sessions
        .iter()
        .filter(|s| s.session_id.starts_with(selector))
        .cloned()
        .collect())
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
            backend: CLAUDE,
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
        let result = resolve_session(None, &sessions).unwrap();
        assert_eq!(result.len(), 2);
        let result = resolve_session(Some("all"), &sessions).unwrap();
        assert_eq!(result.len(), 2);
    }

    #[test]
    fn test_resolve_session_uuid_prefix() {
        let sessions = vec![
            mock_session("aabbccdd-0000-0000-0000-000000000000"),
            mock_session("bbccddee-0000-0000-0000-000000000000"),
        ];
        let result = resolve_session(Some("aabb"), &sessions).unwrap();
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
        let result = resolve_session(Some("1"), &sessions).unwrap();
        assert_eq!(result.len(), 1);
        // offset 2 = newest
        let result2 = resolve_session(Some("2"), &sessions).unwrap();
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
        let result0 = resolve_session(Some("0"), &sessions).unwrap();
        assert_eq!(result0.len(), 1);
        // offset -1 = second latest = same as 0 when there are only 2
        // (offset -1 means "one before latest", so index total-2)
        let result_neg1 = resolve_session(Some("-1"), &sessions).unwrap();
        assert_eq!(result_neg1.len(), 1);
        assert_ne!(result0[0].session_id, result_neg1[0].session_id);
    }

    #[test]
    fn test_resolve_session_offset_out_of_range() {
        let sessions = vec![mock_session("only-session")];
        let err = resolve_session(Some("99"), &sessions).unwrap_err();
        assert!(err.contains("out of range"));
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

    #[test]
    fn test_get_worktree_paths_subdir_of_repo() {
        // A subdirectory of a git repo must NOT inherit the enclosing repo's
        // worktree roots — otherwise its sessions would be conflated with the
        // parent project's. It should return only itself.
        use std::process::Command;
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path().join("repo");
        let sub = repo.join("subdir");
        std::fs::create_dir_all(&sub).unwrap();
        let ok = Command::new("git")
            .args(["init", "-q"])
            .current_dir(&repo)
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
        if !ok {
            return; // git unavailable; nothing to assert
        }
        // canonicalize to match get_worktree_paths' comparison basis
        let sub_canon = std::fs::canonicalize(&sub).unwrap().to_string_lossy().to_string();
        let paths = get_worktree_paths(&sub_canon);
        assert_eq!(paths, vec![sub_canon]);
    }
    #[test]
    fn test_try_verify_decoded_path_existing() {
        let (path, verified) = try_verify_decoded_path("-tmp");
        assert!(verified);
        assert_eq!(path, "/tmp");
    }

    #[test]
    fn test_try_verify_decoded_path_nonexistent() {
        let (path, verified) = try_verify_decoded_path("-absolutely-nonexistent-xyz-123456");
        assert!(!verified);
        assert_eq!(path, "-absolutely-nonexistent-xyz-123456");
    }

    #[test]
    fn test_try_verify_decoded_path_hyphen_in_component() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("my-project");
        std::fs::create_dir(&dir).unwrap();
        let dir_str = dir.to_string_lossy().to_string();
        let encoded = encode_project_path(&dir_str);
        let (path, verified) = try_verify_decoded_path(&encoded);
        assert!(verified, "encoded={}", encoded);
        assert_eq!(path, dir_str);
    }

    #[test]
    fn test_try_verify_decoded_path_dot_in_component() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("my.project");
        std::fs::create_dir(&dir).unwrap();
        let dir_str = dir.to_string_lossy().to_string();
        let encoded = encode_project_path(&dir_str);
        let (path, verified) = try_verify_decoded_path(&encoded);
        assert!(verified, "encoded={}", encoded);
        assert_eq!(path, dir_str);
    }
}
