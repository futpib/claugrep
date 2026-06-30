# claugrep

[![Coverage Status](https://coveralls.io/repos/github/futpib-bot/claugrep/badge.svg?branch=master)](https://coveralls.io/github/futpib-bot/claugrep?branch=master)

Browse, search, and export AI coding-agent conversation transcripts from the command line.

`claugrep` reads transcripts from multiple backends — [Claude Code](https://claude.ai/code)'s per-session `.jsonl` files under `~/.claude/projects/`, [opencode](https://opencode.ai)'s SQLite store at `~/.local/share/opencode/opencode.db`, and any future backend that implements the `Source` trait — and lets you grep across them, list sessions, or dump their content as plain text. By default both are searched (auto-detected); pass `--backend claude` or `--backend opencode` to pick one.

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

## Backends

`claugrep` talks to transcript stores through a `Source` trait (`src/source.rs`). Each backend is one self-contained implementation; a `MultiSource` composes them so the rest of the program is unaware more than one store exists. Adding a backend (codex, …) means dropping one module into `src/backends/` and registering it in `main` — nothing else changes.

| Backend | Storage | Selection |
|---------|---------|-----------|
| `claude` | per-session `.jsonl` under `~/.claude/projects/` (+ claudex accounts) | always, unless `--backend opencode` |
| `opencode` | SQLite at `$XDG_DATA_HOME/opencode/opencode.db` (`session`/`message`/`part` tables) | always, unless `--backend claude`; override path with `--opencode-db` |

`auto` (the default) enables every backend whose store is present, so a single `claugrep search` spans both. `projects`/`sessions` listings annotate each row with its `[backend]` when more than one is active (and `[account]` for multi-account Claude).

**Content-type coverage.** The `-t/--targets` filters work uniformly, but backends differ in what they can populate:

- Both populate: `user`, `assistant`, `thinking`, `bash-command`, `bash-output`, `tool-use`, `tool-result`, `compact-summary`.
- Claude-only (transcript-internal telemetry with no opencode equivalent): `system`, `attachment`, `progress`, `file-history-snapshot`, `queue-operation`, `pull-request`, `bridge-session`, `mode`, and the metadata types. These stay empty for opencode sessions.
- opencode merges a tool call and its result into one record; claugrep still surfaces them as separate `tool-use`/`tool-result` (and `bash-command`/`bash-output`) records so `-t` filtering is identical across backends.
- Tool-name subtypes match case-insensitively, so `-t tool-use.bash` finds both Claude's `Bash` and opencode's `bash`. opencode tool names are otherwise passed through verbatim (including MCP/plugin names like `web-search-prime_web_search_prime`).
- opencode `edit` calls render as unified diffs exactly like Claude's, via the same `EditDiff` path.

`memory dump`/`memory search` honor the backend too: Claude walks `CLAUDE.md` (+ managed policy + auto-memory), opencode walks `AGENTS.md`.

### Global options

These options are accepted by every subcommand. Subcommands ignore options they don't use.

| Flag | Default | Description |
|------|---------|-------------|
| `--backend <which>` | `auto` | Transcript backend(s): `auto` (every backend with data), `claude`, or `opencode` |
| `--opencode-db <path>` | *XDG data dir* | Path to the opencode SQLite DB (`$XDG_DATA_HOME/opencode/opencode.db`) |
| `--config-dir <path>` | `~/.claude` | Claude config directory (overrides `CLAUDE_CONFIG_DIR`) |
| `--account <name>` | | Filter to a specific Claude account (claudex multi-account support) |
| `--color <when>` | `auto` | Colorize output: `auto`, `always`, or `never` (also respects `NO_COLOR`) |
| `--after <date>` / `--since <date>` | | Only show sessions modified after the given date |
| `--before <date>` / `--until <date>` | | Only show sessions modified before the given date |
| `--project <path>` | `.` | Project directory |
| `--all-projects` | | List/search across every known project (rejected by `dump`, `tail`, `memory`) |
| `-P, --project-regexp <REGEXP>` | | Filter to projects matching REGEXP, can be repeated (rejected by `dump`, `tail`, `memory`) |
| `--session <id>` | | Session UUID prefix, numeric offset, or `all` (used by `search`/`dump`/`tail`) |
| `-t, --targets <types>` | `default` | Comma-separated content types (see table below), `default`, or `all` |
| `--no-diff` | | Show raw key/value format for Edit tool records instead of unified diff |
| `--max-line-width <n>` | 200 | Truncate long lines to this width (0 = unlimited) |
| `--max-results <n>` | 50 | Maximum number of results (search / memory search) |
| `--subagents` | | Include subagent transcripts (sessions/last/projects/dump/tail) |
| `--json` | | Output as JSON |

Date values are git-compatible: `yesterday`, `'2 days ago'`, `2026-03-24`, `Monday`, `'last week'`, etc.

The project path is resolved to a canonical absolute path and matched against the directory names in `~/.claude/projects/` and every claudex account.

**Content types** for `-t/--targets`:

| Value | Searches |
|-------|----------|
| `user` | User messages |
| `assistant` | Assistant text responses |
| `thinking` | Assistant thinking blocks |
| `bash-command` | Bash commands sent by the assistant |
| `bash-output` | Bash command output / tool results from Bash |
| `tool-use` | Tool use inputs (any tool) |
| `tool-result` | Tool results (non-Bash tools) |
| `subagent-prompt` | Subagent prompts |
| `compact-summary` | Compact/continuation summaries |
| `queue-operation` | Queue operations |
| `system` | System messages (internal) |
| `file-history-snapshot` | File history snapshots (internal) |

Use `TYPE.SUBTYPE` for narrower filters (e.g. `system.away_summary`, `tool-use.Edit`). The keyword `default` selects standard types; `all` includes internal types.

### `claugrep search`

```
claugrep search [OPTIONS] <PATTERN>
```

Searches transcript content for `PATTERN`. By default the pattern is tried first as a regular expression and, if it is invalid regex, falls back to a literal string match. Accepts the global options listed above.

**Search-specific options:**

| Flag | Default | Description |
|------|---------|-------------|
| `-C, --context <n>` | 0 | Context lines around each match |
| `-B, --before-context <n>` | 0 | Context lines before each match |
| `-A, --after-context <n>` | 0 | Context lines after each match |
| `--around-records <n>` | | Record-level context: N records before + N after the match |
| `--before-records <n>` | | Record-level context: N records immediately before the match |
| `--after-records <n>` | | Record-level context: N records immediately after the match |
| `--records <spec>` | | Record-level context spec: signed offsets & ranges (see below) |
| `--records-type <types>` | | Count and display record context only over these types |
| `-i, --ignore-case` | | Case-insensitive search |
| `-F, --fixed-strings` | | Treat pattern as a fixed string (no regex interpretation) |
| `-E, --extended-regexp` | | Treat pattern as an extended regular expression (no literal fallback) |
| `-l, --list` | | Print only session file paths that contain matches (exits 1 if none); alias `--sessions-with-matches` |

**Edit tool diffs:** When a `tool-use` match is an Edit tool call, the result is rendered as a unified diff by default (old lines prefixed `-`, new lines `+`). Pass `--no-diff` to see the raw `file_path` / `old_string` / `new_string` key-value format instead.

**Record-level context:** `-A/-B/-C` grow the matched *record* with extra *lines* from inside that record. `--around-records`/`--before-records`/`--after-records` instead pull in neighboring *records* of the session (the preceding/following user prompt, assistant turn, tool call, etc.) as additional context blocks around the match.

For precise selection, `--records <SPEC>` accepts:

- a signed integer: `5` (5th record after the match), `-3` (3rd before)
- an inclusive range: `1..5` (records 1 through 5 after), `-3..-1` (3rd through 1st before), `-3..3` (window spanning the match)
- comma-separated combinations: `-3,-1,2,5` or `-5..-3,3..5`

Offset `0` refers to the match itself and is silently ignored — the matched record is always shown.

With `--records-type <types>`, offsets advance only over records of the given types; intermediate records of other types are *hidden* from the output. This is how you ask for "the previous user prompt" (`--records=-1 --records-type=user`) or "the next two assistant turns" (`--records=1..2 --records-type=assistant`), skipping over tool uses and other noise in between.

When `--json` is combined with any record-context flag, each match is wrapped as `{"match": <raw entry>, "context": [{"offset": <n>, "entry": <raw entry>}, ...]}` so consumers can tell match and context apart. Without record-context flags, `--json` keeps its legacy one-raw-entry-per-line format.

**Session offsets:** `--session 0` or `--session -1`, `--session -2` … select relative to the most recent session (0 = latest, -1 = previous, …). `--session 1`, `--session 2` … select from the oldest session forwards (1-based).

**Git worktree support:** When run inside a git repository that has worktrees, `claugrep search` automatically includes sessions from all worktrees of that repository.

### `claugrep sessions`

```
claugrep sessions [GLOBAL OPTIONS]
```

Lists sessions for a project (or every project, with `--all-projects` / `-P`), newest first. Subagent sessions are hidden by default; pass `--subagents` to include them. With `--all-projects`/`-P`, output is grouped by project (per-project header in plain text, `[{project, sessions[]}, ...]` in JSON).

### `claugrep projects`

```
claugrep projects [-s|--sessions]
```

Lists all known projects under `~/.claude/projects/` and every claudex account, showing session count and latest modification time.

| Flag | Description |
|------|-------------|
| `-s, --sessions` | Also list sessions nested under each project (indented in plain text; `sessions` array in JSON) |

When multiple accounts are configured via claudex, projects are annotated with their account name. Pass `--subagents` to include subagent rows in the `--sessions` listing.

### `claugrep last`

```
claugrep last [-n N] [GLOBAL OPTIONS]
```

Shows the last N content records, sorted by timestamp. Defaults to the current project; pass `--all-projects` for the cross-project activity feed (the previous default).

| Flag | Default | Description |
|------|---------|-------------|
| `-n, --last <n>` | 20 | Number of records to show |

### `claugrep dump`

```
claugrep dump [GLOBAL OPTIONS] [SESSION]
```

Dumps the content of a session as plain text. `SESSION` is a UUID prefix, numeric offset (e.g. `-1` for the previous session, `0` for the latest), or `all` (default: `0`). The same selector can be passed as `--session <id>` instead of the positional argument; mixing both forms with non-default values is an error.

`--all-projects` and `-P` are not accepted (single-session command).

### `claugrep tail`

```
claugrep tail [-n N] [-f] [GLOBAL OPTIONS] [SESSION]
```

Shows the last N content records of a session, sorted by timestamp. Optionally follows the session file for new records as they arrive (like `tail -f`). `SESSION` defaults to `0` (the latest session) and may also be supplied via `--session`.

| Flag | Default | Description |
|------|---------|-------------|
| `-n, --lines <n>` | 10 | Number of records to show |
| `-f, --follow` | | Follow the session file for new records (polls every 200 ms) |

`--all-projects` and `-P` are not accepted (single-session command); this also blocks `tail -f --all-projects` transitively.

### `claugrep memory dump`

```
claugrep memory dump [--no-subdirs] [-l|--files-only] [GLOBAL OPTIONS]
```

Prints every markdown memory file (`CLAUDE.md`, auto-memory) that applies to the project. Single-project only — `--all-projects`/`-P` are rejected.

### `claugrep memory search`

```
claugrep memory search [OPTIONS] <PATTERN>
```

Searches markdown memory files for `PATTERN`. Single-project only — `--all-projects`/`-P` are rejected.

| Flag | Description |
|------|-------------|
| `-C/-B/-A` | Line-level context flags (same semantics as `search`) |
| `-i/-F/-E` | Case-insensitive / fixed-string / extended regex |
| `--no-subdirs` | Exclude on-demand `CLAUDE.md` files in subdirectories |
| `-l, --list` | Print only file paths with matches (alias `--files-with-matches`) |

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

# Show 3 surrounding records (whole user/assistant/tool records, not just lines)
claugrep search "serde_json" --around-records 3

# Show the next user prompt after each matching bash command
claugrep search "cargo test" --targets bash-command --records=1 --records-type=user

# Show the previous user prompt and the next one, skipping any tool/assistant records between
claugrep search "error" --records=-1,1 --records-type=user

# Show the 3rd through 5th records after the match
claugrep search "TODO" --records=3..5

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

# Show recent activity in the current project
claugrep last -n 10

# Show recent activity across all projects
claugrep last -n 10 --all-projects

# List sessions across every project (grouped by project)
claugrep sessions --all-projects

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

# ── opencode backend ────────────────────────────────────────────────────────

# Search opencode sessions only (skip the opencode.db auto-detection)
claugrep --backend opencode search "auth" -t user

# Dump the latest opencode session's bash commands + outputs
claugrep --backend opencode dump 0 -t bash-command,bash-output

# List every project claugrep can see (both backends, tagged)
claugrep projects
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

### Adding a backend

Implement the `Source` trait (`src/source.rs`) — `discover_projects`, `discover_sessions`, `extract_content`, `follow`, and `discover_memory_files` — in a new `src/backends/<name>.rs`, returning the shared `ExtractedContent` / `SessionFile` / `ProjectInfo` types. Register an instance in `main`'s source-construction block (and add it to the `--backend` enum if you want explicit selection). `MultiSource` handles merging and per-session dispatch automatically; no other code needs to change.
