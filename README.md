# claugrep

[![Coverage Status](https://coveralls.io/repos/github/futpib-bot/claugrep/badge.svg?branch=master)](https://coveralls.io/github/futpib-bot/claugrep?branch=master)

Browse, search, and export Claude Code conversation transcripts from the command line.

`claugrep` reads the JSONL session files written by [Claude Code](https://claude.ai/code) to `~/.claude/projects/` and lets you grep across them, list sessions, or dump their content as plain text.

## Installation

### From source

```sh
cargo install --path .
```

### AUR (Arch Linux)

```sh
yay -S claugrep-git
# or
paru -S claugrep-git
```

## Usage

```
claugrep <COMMAND>

Commands:
  search    Search Claude Code conversation transcripts
  sessions  List sessions for a project
  dump      Dump a session's content as plain text
```

All commands accept `--project <path>` to select which project's sessions to search (default: current directory). The project path is resolved to a canonical absolute path and matched against the directory names in `~/.claude/projects/`.

### `claugrep search`

```
claugrep search [OPTIONS] <PATTERN>
```

Searches transcript content for `PATTERN`. The pattern is interpreted both as a literal string and as a regular expression — both interpretations are applied and their match results are unioned.

**Content type filters** (default: all types):

| Flag | Searches |
|------|----------|
| `-u, --user` | User messages |
| `-a, --assistant` | Assistant text responses |
| `-c, --bash-command` | Bash commands sent by the assistant |
| `-o, --bash-output` | Bash command output / tool results from Bash |
| `-t, --tool-use` | Tool use inputs (any tool) |
| `-r, --tool-result` | Tool results (non-Bash tools) |
| `-s, --subagent-prompt` | Subagent prompts |
| `--compact-summary` | Compact/continuation summaries |

**Other options:**

| Flag | Default | Description |
|------|---------|-------------|
| `--project <path>` | `.` | Project directory |
| `--session <id>` | all | Session UUID prefix, numeric offset, or `all` |
| `-C, --context <n>` | 0 | Context lines around each match |
| `-B, --before-context <n>` | 0 | Context lines before each match |
| `-A, --after-context <n>` | 0 | Context lines after each match |
| `--max-results <n>` | 50 | Maximum number of matches to return |
| `--max-line-width <n>` | 200 | Truncate long lines to this width (0 = unlimited) |
| `-i, --ignore-case` | | Case-insensitive search |
| `-l, --sessions-with-matches` | | Print only session file paths that contain matches (exits 1 if none) |
| `--json` | | Output matches as JSON |

**Session offsets:** `--session 0` or `--session -1`, `--session -2` … select relative to the most recent session (0 = latest, -1 = previous, …). `--session 1`, `--session 2` … select from the oldest session forwards (1-based).

**Git worktree support:** When run inside a git repository that has worktrees, `claugrep search` automatically includes sessions from all worktrees of that repository.

### `claugrep sessions`

```
claugrep sessions [--project <path>] [--json]
```

Lists all sessions for a project, newest first. Subagent sessions are omitted from the default output but included in JSON output (`isSubagent: true`).

### `claugrep dump`

```
claugrep dump [--project <path>] [--targets <types>] <SESSION>
```

Dumps the content of a session as plain text. `SESSION` is a UUID prefix, numeric offset (e.g. `-1` for the previous session, `0` for the latest), or `all`.

`--targets` is a comma-separated list of content types to include (default: `user,assistant`). Valid types: `user`, `assistant`, `bash-command`, `bash-output`, `tool-use`, `tool-result`, `subagent-prompt`, `compact-summary`.

## Examples

```sh
# Search user messages across all sessions in the current project
claugrep search "cargo build" --user

# Search for a regex pattern in bash commands
claugrep search "git (push|pull)" --bash-command

# Case-insensitive search in assistant responses
claugrep search "TODO" --assistant --ignore-case

# Show 2 context lines around each match
claugrep search "serde_json" -C 2

# Search a specific project
claugrep search "feature request" --user --project ~/code/my-project

# Search only the most recent session
claugrep search "error" --session 0

# Search only the previous session (second most recent)
claugrep search "error" --session -1

# List all session IDs that mention "tmux"
claugrep search "tmux" --user --sessions-with-matches

# Output matches as JSON for scripting
claugrep search "regex" --json | jq '.[].matchedLines[].line'

# List sessions for a project
claugrep sessions --project ~/code/my-project

# Dump the latest session (user + assistant messages)
claugrep dump 0 --project ~/code/my-project

# Dump all bash commands from the previous session
claugrep dump -1 --targets bash-command --project ~/code/my-project

# Dump everything from session with UUID prefix abc123
claugrep dump abc123 --targets user,assistant,bash-command,bash-output
```

## Feature parity with claudex

`claugrep` implements the same search functionality as the [`claudex memory search`](https://github.com/futpib/claudex) command, plus additional `sessions` and `dump` subcommands. Both tools:

- Parse the same Claude Code JSONL transcript format
- Support the same content-type filters and search flags
- Apply both literal and regex interpretations of the search pattern
- Deduplicate sessions across git worktrees
- Emit a truncation hint to stderr when `--max-line-width` causes lines to be cut

## Development

```sh
# Build
cargo build

# Run unit tests
cargo test --bin claugrep

# Run all tests (including integration tests against ~/.claude/projects/)
cargo test
```

Integration tests in `tests/integration.rs` run the binary against real Claude Code session transcripts. They skip gracefully in environments where no transcripts exist.
