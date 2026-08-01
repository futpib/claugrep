//! Hermetic end-to-end coverage for Codex's native memory store.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

struct CodexMemoryWorld {
    root: tempfile::TempDir,
    codex_home: PathBuf,
    project: PathBuf,
}

impl CodexMemoryWorld {
    fn new() -> Self {
        let root = tempfile::tempdir().unwrap();
        let codex_home = root.path().join("codex-home");
        let project = root.path().join("project");
        fs::create_dir_all(codex_home.join("memories/extensions/import/resources")).unwrap();
        fs::create_dir_all(&project).unwrap();

        fs::write(
            codex_home.join("memories/memory_summary.md"),
            "# Summary\nsummary-only-needle\n",
        )
        .unwrap();
        fs::write(
            codex_home.join("memories/MEMORY.md"),
            "# Registry\nregistry-only-needle\n",
        )
        .unwrap();
        fs::write(
            codex_home.join("memories/extensions/import/resources/topic.md"),
            "# Imported topic\nnative-codex-needle\n",
        )
        .unwrap();
        fs::write(
            codex_home.join("memories/phase2_workspace_diff.md"),
            "native-codex-needle must not leak from here\n",
        )
        .unwrap();

        Self {
            root,
            codex_home,
            project,
        }
    }

    fn cmd(&self) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_claugrep"));
        command
            .env("HOME", self.root.path())
            .env("NO_COLOR", "1")
            .args(["--backend", "codex", "--codex-home"])
            .arg(&self.codex_home)
            .arg("--project")
            .arg(&self.project);
        command
    }
}

fn json_path(value: &serde_json::Value) -> &Path {
    Path::new(value["path"].as_str().unwrap())
}

#[test]
fn memory_dump_reads_native_store_without_sessions() {
    let world = CodexMemoryWorld::new();
    let output = world
        .cmd()
        .args(["--json", "memory", "dump", "--no-subdirs"])
        .output()
        .unwrap();

    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let files: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    let files = files.as_array().unwrap();
    assert_eq!(files.len(), 3);
    assert_eq!(
        json_path(&files[0])
            .file_name()
            .and_then(|name| name.to_str()),
        Some("memory_summary.md")
    );
    assert_eq!(files[0]["source"], "codex-memory-summary");
    assert_eq!(
        json_path(&files[1])
            .file_name()
            .and_then(|name| name.to_str()),
        Some("MEMORY.md")
    );
    assert_eq!(files[1]["source"], "codex-memory-registry");
    assert!(json_path(&files[2]).ends_with("extensions/import/resources/topic.md"));
    assert_eq!(files[2]["source"], "codex-memory-file");
    assert!(files
        .iter()
        .all(|file| !json_path(file).ends_with("phase2_workspace_diff.md")));
}

#[test]
fn memory_search_finds_nested_native_resources() {
    let world = CodexMemoryWorld::new();
    let output = world
        .cmd()
        .args(["memory", "search", "native-codex-needle", "--no-subdirs"])
        .output()
        .unwrap();

    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("extensions/import/resources/topic.md"));
    assert!(stdout.contains("[codex-memory-file]"));
    assert!(!stdout.contains("phase2_workspace_diff.md"));
}
