use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};

use regex::Regex;

/// Where a memory file was discovered — used for ordering and for labels in output.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MemorySource {
    /// System-wide managed policy (`/etc/claude-code/CLAUDE.md`).
    ManagedPolicy,
    /// User global (`<config_dir>/CLAUDE.md` or `<config_dir>/CLAUDE.local.md`).
    UserGlobal,
    /// A `CLAUDE.md` / `CLAUDE.local.md` walking up from cwd (includes the project root).
    Ancestor,
    /// A `CLAUDE.md` / `CLAUDE.local.md` in a subdirectory of the project (loaded on demand).
    Subdir,
    /// The `MEMORY.md` index of the auto-memory system.
    AutoMemoryIndex,
    /// A topic file in the auto-memory directory (loaded on demand).
    AutoMemoryTopic,
    /// Imported inline via `@path` from another memory file.
    Import,
}

impl MemorySource {
    pub fn label(self) -> &'static str {
        match self {
            MemorySource::ManagedPolicy    => "managed-policy",
            MemorySource::UserGlobal       => "user-global",
            MemorySource::Ancestor         => "ancestor",
            MemorySource::Subdir           => "subdir",
            MemorySource::AutoMemoryIndex  => "auto-memory-index",
            MemorySource::AutoMemoryTopic  => "auto-memory-topic",
            MemorySource::Import           => "import",
        }
    }
}

#[derive(Debug, Clone)]
pub struct MemoryFile {
    pub path: PathBuf,
    pub source: MemorySource,
    pub imported_by: Option<PathBuf>,
}

/// Project-path encoding used by Claude for the auto-memory directory name.
fn encode_project_path(path: &str) -> String {
    path.replace(['/', '.'], "-")
}

/// Directories skipped during the on-demand subdirectory scan. Matches the common
/// ignore set Claude itself doesn't descend into, plus VCS/build output.
fn is_ignored_dir(name: &str) -> bool {
    matches!(
        name,
        ".git" | "node_modules" | "target" | "dist" | "build"
        | ".next" | ".nuxt" | ".cache" | ".venv" | "venv" | "__pycache__"
        | ".idea" | ".vscode"
    )
}

/// Discover every markdown memory file that would apply to `cwd` if a Claude Code
/// session started there, in load order:
///   1. Managed policy
///   2. User global (+ imports) — per config dir
///   3. Ancestor CLAUDE.md files from root down to cwd (+ imports)
///   4. Subdir CLAUDE.md files (on-demand)
///   5. Auto-memory index + topic files — per config dir
///
/// `config_dirs` lets callers pass the default plus every claudex account root so
/// per-account user-global files and per-account auto-memory dirs are all included.
/// `cwd` should already be canonicalized by the caller.
pub fn discover_memory_files(cwd: &Path, config_dirs: &[&Path], include_subdirs: bool) -> Vec<MemoryFile> {
    let mut seen: HashSet<PathBuf> = HashSet::new();
    let mut result: Vec<MemoryFile> = Vec::new();

    let push = |result: &mut Vec<MemoryFile>, seen: &mut HashSet<PathBuf>, file: MemoryFile| {
        if seen.insert(file.path.clone()) {
            result.push(file);
        }
    };

    // 1. Managed policy
    let managed = PathBuf::from("/etc/claude-code/CLAUDE.md");
    if managed.is_file() {
        push(&mut result, &mut seen, MemoryFile {
            path: managed.clone(), source: MemorySource::ManagedPolicy, imported_by: None,
        });
        for imp in collect_imports(&managed, 5) {
            push(&mut result, &mut seen, MemoryFile {
                path: imp, source: MemorySource::Import, imported_by: Some(managed.clone()),
            });
        }
    }

    // 2. User global — one pass per config dir
    for config_dir in config_dirs {
        for name in ["CLAUDE.md", "CLAUDE.local.md"] {
            let p = config_dir.join(name);
            if p.is_file() {
                push(&mut result, &mut seen, MemoryFile {
                    path: p.clone(), source: MemorySource::UserGlobal, imported_by: None,
                });
                for imp in collect_imports(&p, 5) {
                    push(&mut result, &mut seen, MemoryFile {
                        path: imp, source: MemorySource::Import, imported_by: Some(p.clone()),
                    });
                }
            }
        }
    }

    // 3. Walk up from cwd to /, collecting CLAUDE.md files. Emit far-to-near.
    let mut ancestors: Vec<PathBuf> = Vec::new();
    {
        let mut current = Some(cwd.to_path_buf());
        while let Some(dir) = current {
            ancestors.push(dir.clone());
            current = dir.parent().map(|p| p.to_path_buf());
        }
    }
    ancestors.reverse();
    for dir in &ancestors {
        for name in ["CLAUDE.md", "CLAUDE.local.md"] {
            let p = dir.join(name);
            if p.is_file() && !seen.contains(&p) {
                push(&mut result, &mut seen, MemoryFile {
                    path: p.clone(), source: MemorySource::Ancestor, imported_by: None,
                });
                for imp in collect_imports(&p, 5) {
                    push(&mut result, &mut seen, MemoryFile {
                        path: imp, source: MemorySource::Import, imported_by: Some(p.clone()),
                    });
                }
            }
        }
    }

    // 4. On-demand subdir CLAUDE.md files (skipped if opted-out).
    if include_subdirs {
        for p in find_subdir_claude_md(cwd) {
            if !seen.contains(&p) {
                push(&mut result, &mut seen, MemoryFile {
                    path: p.clone(), source: MemorySource::Subdir, imported_by: None,
                });
                for imp in collect_imports(&p, 5) {
                    push(&mut result, &mut seen, MemoryFile {
                        path: imp, source: MemorySource::Import, imported_by: Some(p.clone()),
                    });
                }
            }
        }
    }

    // 5. Auto-memory — one pass per config dir
    let cwd_str = cwd.to_string_lossy();
    for config_dir in config_dirs {
        let project_mem_dir = config_dir
            .join("projects")
            .join(encode_project_path(&cwd_str))
            .join("memory");

        let index = project_mem_dir.join("MEMORY.md");
        if index.is_file() {
            push(&mut result, &mut seen, MemoryFile {
                path: index, source: MemorySource::AutoMemoryIndex, imported_by: None,
            });
        }
        if let Ok(entries) = fs::read_dir(&project_mem_dir) {
            let mut topic: Vec<PathBuf> = entries.flatten()
                .map(|e| e.path())
                .filter(|p| p.is_file()
                    && p.extension().map(|e| e == "md").unwrap_or(false)
                    && p.file_name().map(|n| n != "MEMORY.md").unwrap_or(false))
                .collect();
            topic.sort();
            for p in topic {
                push(&mut result, &mut seen, MemoryFile {
                    path: p, source: MemorySource::AutoMemoryTopic, imported_by: None,
                });
            }
        }
    }

    result
}

/// Recursively find `CLAUDE.md` / `CLAUDE.local.md` under `root`, excluding the
/// immediate root (those are emitted as Ancestor) and common ignore directories.
fn find_subdir_claude_md(root: &Path) -> Vec<PathBuf> {
    let mut out: Vec<PathBuf> = Vec::new();
    let mut stack: Vec<PathBuf> = vec![root.to_path_buf()];

    while let Some(dir) = stack.pop() {
        let Ok(entries) = fs::read_dir(&dir) else { continue };
        for entry in entries.flatten() {
            let path = entry.path();
            let Ok(ft) = entry.file_type() else { continue };

            if ft.is_dir() {
                let name = entry.file_name();
                let name_s = name.to_string_lossy();
                if is_ignored_dir(&name_s) { continue }
                stack.push(path);
            } else if ft.is_file() {
                let Some(fname) = path.file_name().and_then(|n| n.to_str()) else { continue };
                if (fname == "CLAUDE.md" || fname == "CLAUDE.local.md")
                    && path.parent() != Some(root)
                {
                    out.push(path);
                }
            }
        }
    }

    out.sort();
    out
}

/// Parse `@path` imports out of `file` and recursively follow them, stopping at
/// `max_depth` hops. Returns the set of imported file paths (not including
/// `file` itself), in stable order.
pub fn collect_imports(file: &Path, max_depth: u32) -> Vec<PathBuf> {
    let mut seen: HashSet<PathBuf> = HashSet::new();
    seen.insert(file.to_path_buf());
    let mut order: Vec<PathBuf> = Vec::new();
    collect_imports_rec(file, max_depth, &mut seen, &mut order);
    order
}

fn collect_imports_rec(
    file: &Path,
    depth_left: u32,
    seen: &mut HashSet<PathBuf>,
    order: &mut Vec<PathBuf>,
) {
    if depth_left == 0 { return }
    let Ok(content) = fs::read_to_string(file) else { return };
    let base_dir = file.parent().unwrap_or_else(|| Path::new("."));
    for target in parse_import_refs(&content) {
        let resolved = match resolve_import_path(&target, base_dir) {
            Some(p) => p,
            None => continue,
        };
        if !resolved.is_file() { continue }
        if seen.insert(resolved.clone()) {
            order.push(resolved.clone());
            collect_imports_rec(&resolved, depth_left - 1, seen, order);
        }
    }
}

/// Find `@<path>` tokens. Conservative: requires the target to end in `.md` or
/// contain a path separator — this avoids false positives from GitHub mentions
/// (`@user`) or generic `@foo` strings.
fn parse_import_refs(content: &str) -> Vec<String> {
    let re = Regex::new(r"(?m)(?:^|[\s(])@([^\s()\[\]<>'\x22]+)").unwrap();
    let mut out: Vec<String> = Vec::new();
    for cap in re.captures_iter(content) {
        let target = cap.get(1).unwrap().as_str();
        let looks_like_path = target.contains('/')
            || target.starts_with('~')
            || target.ends_with(".md");
        if looks_like_path {
            out.push(target.to_string());
        }
    }
    out
}

fn resolve_import_path(target: &str, base: &Path) -> Option<PathBuf> {
    if let Some(rest) = target.strip_prefix("~/") {
        return Some(dirs::home_dir()?.join(rest));
    }
    if target == "~" {
        return dirs::home_dir();
    }
    let p = PathBuf::from(target);
    if p.is_absolute() { Some(p) } else { Some(base.join(p)) }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encode_matches_sessions_encoding() {
        assert_eq!(encode_project_path("/home/alice"), "-home-alice");
        assert_eq!(encode_project_path("/home/alice/my.proj"), "-home-alice-my-proj");
    }

    #[test]
    fn parse_import_refs_finds_md_targets() {
        let content = "see @./foo.md and @~/bar.md but ignore @user or @foo\n";
        let refs = parse_import_refs(content);
        assert_eq!(refs, vec!["./foo.md", "~/bar.md"]);
    }

    #[test]
    fn parse_import_refs_allows_path_without_md_suffix() {
        let content = "@sub/dir/instructions\n";
        let refs = parse_import_refs(content);
        assert_eq!(refs, vec!["sub/dir/instructions"]);
    }

    #[test]
    fn collect_imports_follows_chain_up_to_depth() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        fs::write(root.join("a.md"), "@./b.md\n").unwrap();
        fs::write(root.join("b.md"), "@./c.md\n").unwrap();
        fs::write(root.join("c.md"), "leaf\n").unwrap();

        let got = collect_imports(&root.join("a.md"), 5);
        let names: Vec<_> = got.iter().filter_map(|p| p.file_name()).map(|n| n.to_string_lossy().into_owned()).collect();
        assert_eq!(names, vec!["b.md".to_string(), "c.md".to_string()]);
    }

    #[test]
    fn collect_imports_handles_cycles() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        fs::write(root.join("a.md"), "@./b.md\n").unwrap();
        fs::write(root.join("b.md"), "@./a.md\n").unwrap();

        let got = collect_imports(&root.join("a.md"), 5);
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].file_name().unwrap(), "b.md");
    }

    #[test]
    fn discover_walks_ancestors_and_subdirs() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        let config_dir = root.join(".claude");
        fs::create_dir_all(&config_dir).unwrap();
        fs::write(config_dir.join("CLAUDE.md"), "global\n").unwrap();

        let project = root.join("project");
        fs::create_dir_all(&project).unwrap();
        fs::write(project.join("CLAUDE.md"), "project\n").unwrap();

        let sub = project.join("sub");
        fs::create_dir_all(&sub).unwrap();
        fs::write(sub.join("CLAUDE.md"), "sub\n").unwrap();

        let files = discover_memory_files(&project, &[config_dir.as_path()], true);
        let names: Vec<_> = files.iter()
            .map(|f| (f.path.file_name().unwrap().to_string_lossy().into_owned(), f.source))
            .collect();
        assert!(names.contains(&("CLAUDE.md".to_string(), MemorySource::UserGlobal)),
            "user-global missing: {:?}", names);
        assert!(names.iter().any(|(n, s)| n == "CLAUDE.md" && *s == MemorySource::Ancestor),
            "ancestor missing: {:?}", names);
        assert!(names.iter().any(|(n, s)| n == "CLAUDE.md" && *s == MemorySource::Subdir),
            "subdir missing: {:?}", names);
    }

    #[test]
    fn discover_skips_subdirs_when_opted_out() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        let config_dir = root.join(".claude");
        fs::create_dir_all(&config_dir).unwrap();
        let project = root.join("project");
        let sub = project.join("sub");
        fs::create_dir_all(&sub).unwrap();
        fs::write(project.join("CLAUDE.md"), "project\n").unwrap();
        fs::write(sub.join("CLAUDE.md"), "sub\n").unwrap();

        let files = discover_memory_files(&project, &[config_dir.as_path()], false);
        assert!(!files.iter().any(|f| f.source == MemorySource::Subdir));
    }

    #[test]
    fn discover_includes_auto_memory() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        let config_dir = root.join(".claude");
        let project = root.join("proj");
        fs::create_dir_all(&project).unwrap();

        let encoded = encode_project_path(&project.to_string_lossy());
        let mem_dir = config_dir.join("projects").join(&encoded).join("memory");
        fs::create_dir_all(&mem_dir).unwrap();
        fs::write(mem_dir.join("MEMORY.md"), "- index\n").unwrap();
        fs::write(mem_dir.join("topic_a.md"), "a\n").unwrap();
        fs::write(mem_dir.join("topic_b.md"), "b\n").unwrap();

        let files = discover_memory_files(&project, &[config_dir.as_path()], false);
        let auto: Vec<_> = files.iter()
            .filter(|f| matches!(f.source, MemorySource::AutoMemoryIndex | MemorySource::AutoMemoryTopic))
            .map(|f| (f.path.file_name().unwrap().to_string_lossy().into_owned(), f.source))
            .collect();
        assert_eq!(auto.len(), 3);
        assert_eq!(auto[0].1, MemorySource::AutoMemoryIndex);
        assert_eq!(auto[0].0, "MEMORY.md");
        assert!(auto[1..].iter().all(|(_, s)| *s == MemorySource::AutoMemoryTopic));
    }

    #[test]
    fn discover_merges_across_multiple_config_dirs() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        let default_cfg = root.join(".claude");
        let account_cfg = root.join("claudex/accounts/archive/claude");
        fs::create_dir_all(&default_cfg).unwrap();
        fs::create_dir_all(&account_cfg).unwrap();
        fs::write(default_cfg.join("CLAUDE.md"), "default-global\n").unwrap();
        fs::write(account_cfg.join("CLAUDE.md"), "archive-global\n").unwrap();

        let project = root.join("proj");
        fs::create_dir_all(&project).unwrap();
        let encoded = encode_project_path(&project.to_string_lossy());

        let default_mem = default_cfg.join("projects").join(&encoded).join("memory");
        fs::create_dir_all(&default_mem).unwrap();
        fs::write(default_mem.join("MEMORY.md"), "- default\n").unwrap();
        fs::write(default_mem.join("topic_default.md"), "d\n").unwrap();

        let account_mem = account_cfg.join("projects").join(&encoded).join("memory");
        fs::create_dir_all(&account_mem).unwrap();
        fs::write(account_mem.join("MEMORY.md"), "- archive\n").unwrap();
        fs::write(account_mem.join("topic_archive.md"), "a\n").unwrap();

        let files = discover_memory_files(
            &project,
            &[default_cfg.as_path(), account_cfg.as_path()],
            false,
        );

        let globals: Vec<_> = files.iter()
            .filter(|f| f.source == MemorySource::UserGlobal)
            .map(|f| f.path.clone())
            .collect();
        assert!(globals.contains(&default_cfg.join("CLAUDE.md")),
            "default user-global missing: {:?}", globals);
        assert!(globals.contains(&account_cfg.join("CLAUDE.md")),
            "account user-global missing: {:?}", globals);

        let auto_paths: Vec<_> = files.iter()
            .filter(|f| matches!(f.source,
                MemorySource::AutoMemoryIndex | MemorySource::AutoMemoryTopic))
            .map(|f| f.path.clone())
            .collect();
        assert!(auto_paths.contains(&default_mem.join("MEMORY.md")));
        assert!(auto_paths.contains(&default_mem.join("topic_default.md")));
        assert!(auto_paths.contains(&account_mem.join("MEMORY.md")));
        assert!(auto_paths.contains(&account_mem.join("topic_archive.md")));
    }
}
