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
claugrep [OPTIONS] <COMMAND>

Commands:
  search    Search Claude Code conversation transcripts
  sessions  List sessions for a project
  last      Show the last N records across all sessions, sorted by time
  projects  List all known projects under ~/.claude/projects/
  dump      Dump a session's content as plain text
  tail      Show the last N records of a session (like tail)
```

### Global options

These options are accepted by all commands:

| Flag | Default | Description |
|------|---------|-------------|
| `--config-dir <path>` | `~/.claude` | Claude config directory (overrides `CLAUDE_CONFIG_DIR`) |
| `--account <name>` | | Filter to a specific account (claudex multi-account support) |
| `--color <when>` | `auto` | Colorize output: `auto`, `always`, or `never` (also respects `NO_COLOR`) |
| `--after <date>` / `--since <date>` | | Only show sessions modified after the given date |
| `--before <date>` / `--until <date>` | | Only show sessions modified before the given date |

Date values are git-compatible: `yesterday`, `'2 days ago'`, `2026-03-24`, `Monday`, `'last week'`, etc.

Most commands accept `--project <path>` to select which project's sessions to use (default: current directory). The project path is resolved to a canonical absolute path and matched against the directory names in `~/.claude/projects/`.

### `claugrep search`

```
claugrep search [OPTIONS] <PATTERN>
```

Searches transcript content for `PATTERN`. By default the pattern is tried first as a regular expression and, if it is invalid regex, falls back to a literal string match.

**Content type filter** (default: standard types):

| Value | Searches |
|-------|----------|
| `user` | User messages |
| `assistant` | Assistant text responses |
| `bash-command` | Bash commands sent by the assistant |
| `bash-output` | Bash command output / tool results from Bash |
| `tool-use` | Tool use inputs (any tool) |
| `tool-result` | Tool results (non-Bash tools) |
| `subagent-prompt` | Subagent prompts |
| `compact-summary` | Compact/continuation summaries |
| `queue-operation` | Queue operations |
| `system` | System messages (internal) |
| `file-history-snapshot` | File history snapshots (internal) |

Pass one or more types as a comma-separated value to `-t/--targets`, e.g. `--targets user,assistant`. Use the special keyword `default` (the default) for all standard types, or `all` to include internal types as well.

**Options:**

| Flag | Default | Description |
|------|---------|-------------|
| `-t, --targets <types>` | `default` | Comma-separated content types (see table above), `default`, or `all` |
| `--project <path>` | `.` | Project directory |
| `--session <id>` | all | Session UUID prefix, numeric offset, or `all` |
| `-C, --context <n>` | 0 | Context lines around each match |
| `-B, --before-context <n>` | 0 | Context lines before each match |
| `-A, --after-context <n>` | 0 | Context lines after each match |
| `--max-results <n>` | 50 | Maximum number of matches to return |
| `--max-line-width <n>` | 200 | Truncate long lines to this width (0 = unlimited) |
| `-i, --ignore-case` | | Case-insensitive search |
| `-F, --fixed-strings` | | Treat pattern as a fixed string (no regex interpretation) |
| `-E, --extended-regexp` | | Treat pattern as an extended regular expression (no literal fallback) |
| `-l, --sessions-with-matches` | | Print only session file paths that contain matches (exits 1 if none) |
| `--no-diff` | | For Edit tool matches, show raw key/value format instead of unified diff |
| `--json` | | Output matches as JSON |
| `--all-projects` | | Search all projects under `~/.claude/projects/` (ignores `--project`) |
| `-P, --project-regexp <REGEXP>` | | Search only projects whose path matches REGEXP; can be repeated (ignores `--project`) |

**Edit tool diffs:** When a `tool-use` match is an Edit tool call, the result is rendered as a unified diff by default (old lines prefixed `-`, new lines `+`). Pass `--no-diff` to see the raw `file_path` / `old_string` / `new_string` key-value format instead.

**Session offsets:** `--session 0` or `--session -1`, `--session -2` … select relative to the most recent session (0 = latest, -1 = previous, …). `--session 1`, `--session 2` … select from the oldest session forwards (1-based).

**Git worktree support:** When run inside a git repository that has worktrees, `claugrep search` automatically includes sessions from all worktrees of that repository.

### `claugrep sessions`

```
claugrep sessions [--project <path>] [--json]
```

Lists all sessions for a project, newest first. Subagent sessions are omitted from the default output but included in JSON output (`isSubagent: true`).

### `claugrep projects`

```
claugrep projects [--sessions] [--json]
```

Lists all known projects under `~/.claude/projects/`, showing session count and latest modification time.

| Flag | Description |
|------|-------------|
| `-s, --sessions` | Also list sessions nested under each project (indented in plain text; `sessions` array in JSON) |
| `--json` | Output as JSON |

When multiple accounts are configured via claudex, projects are annotated with their account name.

### `claugrep last`

```
claugrep last [OPTIONS]
```

Shows the last N content records across all sessions (or all sessions of a given project), sorted by timestamp. Useful for a quick cross-project activity feed.

| Flag | Default | Description |
|------|---------|-------------|
| `-n, --last <n>` | 20 | Number of records to show |
| `--project <path>` | all projects | Restrict to a specific project |
| `-t, --targets <types>` | `default` | Comma-separated content types, `default`, or `all` |
| `--max-line-width <n>` | 200 | Truncate long lines (0 = unlimited) |
| `--no-diff` | | Show raw key/value format for Edit tool records |
| `--json` | | Output raw JSONL records |

### `claugrep dump`

```
claugrep dump [OPTIONS] [SESSION]
```

Dumps the content of a session as plain text. `SESSION` is a UUID prefix, numeric offset (e.g. `-1` for the previous session, `0` for the latest), or `all` (default: `0`).

| Flag | Default | Description |
|------|---------|-------------|
| `--project <path>` | `.` | Project directory |
| `-t, --targets <types>` | `default` | Comma-separated content types, `default`, or `all` |
| `--no-diff` | | Show raw key/value format for Edit tool records |
| `--json` | | Output raw JSONL records |

Valid target types: `user`, `assistant`, `bash-command`, `bash-output`, `tool-use`, `tool-result`, `subagent-prompt`, `compact-summary`, `queue-operation`, `system`, `file-history-snapshot`, `last-prompt`.

### `claugrep tail`

```
claugrep tail [OPTIONS] [SESSION]
```

Shows the last N content records of a session, sorted by timestamp. Optionally follows the session file for new records as they arrive (like `tail -f`). `SESSION` defaults to `0` (the latest session).

| Flag | Default | Description |
|------|---------|-------------|
| `-n, --lines <n>` | 10 | Number of records to show |
| `-f, --follow` | | Follow the session file for new records (polls every 200 ms) |
| `--project <path>` | `.` | Project directory |
| `-t, --targets <types>` | `default` | Comma-separated content types, `default`, or `all` |
| `--max-line-width <n>` | 200 | Truncate long lines (0 = unlimited) |
| `--no-diff` | | Show raw key/value format for Edit tool records |
| `--json` | | Output raw JSONL records |

## Examples

```sh
# Search user messages across all sessions in the current project
claugrep search "cargo build" --targets user

# Search for a regex pattern in bash commands
claugrep search "git (push|pull)" --targets bash-command

# Case-insensitive search in assistant responses
claugrep search "TODO" --targets assistant --ignore-case

# Search for a literal string (no regex)
claugrep search "file[0]" --fixed-strings

# Show 2 context lines around each match
claugrep search "serde_json" -C 2

# Search a specific project
claugrep search "feature request" --targets user --project ~/code/my-project

# Search only the most recent session
claugrep search "error" --session 0

# Search only the previous session (second most recent)
claugrep search "error" --session -1

# List all session file paths that mention "tmux"
claugrep search "tmux" --targets user --sessions-with-matches

# Output matches as JSON for scripting
claugrep search "regex" --json | jq '.[].matchedLines[].line'

# Search across all projects
claugrep search "TODO" --all-projects

# Search only projects whose path matches a pattern
claugrep search "fix" --project-regexp "my-project|other-project"

# Filter sessions by date
claugrep search "error" --after yesterday
claugrep search "error" --after "2 days ago" --before today

# List sessions for a project
claugrep sessions --project ~/code/my-project

# List all known projects
claugrep projects

# List all projects with their sessions
claugrep projects --sessions

# Show recent activity across all projects
claugrep last -n 10

# Dump the latest session (user + assistant messages)
claugrep dump 0 --project ~/code/my-project

# Dump all bash commands from the previous session
claugrep dump -1 --targets bash-command --project ~/code/my-project

# Dump everything from session with UUID prefix abc123
claugrep dump abc123 --targets user,assistant,bash-command,bash-output

# Show the last 5 records of the current session
claugrep tail -n 5

# Follow the current session live (like tail -f)
claugrep tail -f
```

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
