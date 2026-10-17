# zeron-tools

Offline tools for a local [Zeron](https://github.com/zeronsh/zeron) profile:
- import pi sessions and Codex threads (for example from the ChatGPT/Codex app) that were created outside Zeron
- retitle chats with a Codex model
- export chats as Markdown or JSONL, for example to have an agent review them

Supports macOS on Apple silicon, the same as Zeron. See `AGENTS.md` for how it works and how to update it for a new Zeron release.

## Installation

```sh
curl -fsSL https://raw.githubusercontent.com/iyazerski/zeron-tools/main/install.sh | sh
```

This installs the latest release to `~/.local/bin/zeron-tools`. Each release targets one Zeron version, shown by `zeron-tools --version`. If your Zeron is older, pin a matching release:

```sh
curl -fsSL https://raw.githubusercontent.com/iyazerski/zeron-tools/main/install.sh | ZERON_TOOLS_VERSION=v0.1.0 sh
```

To build from source instead, run `make install`.

## Usage

```sh
zeron-tools sessions codex                 # sessions not in Zeron yet (works while Zeron runs)
zeron-tools sessions pi --all              # also linked and hidden ones
zeron-tools import codex 019e5f9c-1bf2 --dry-run

# Export (works while Zeron runs)
zeron-tools export --project guid-service --since 30d -o ~/tmp/guid-review
zeron-tools export --chat 88bf3b20 --reasoning > chat.md
zeron-tools export --since 2026-09-01 --until 2026-09-30 --limit 20 --format jsonl

# Writes: quit Zeron (Cmd+Q) first. Each write backs up docs.sqlite3
# to ~/.zeron/zeron-tools-backups/<time>/ first.
zeron-tools import codex 019e5f9c-1bf2 019d80ca-bcc6 --create-project
zeron-tools import pi 01a0f866-31fb
zeron-tools rename --untitled              # or --chat <id-prefix>..., or --all
open -a Zeron
```

- **Sources:** pi sessions come from `~/.pi/agent/sessions` (or `PI_CODING_AGENT_DIR`). Codex threads come from `~/.codex` (or `CODEX_HOME`), including archived ones, which import as archived chats.
- **Hidden by `sessions`:** subagent threads, `codex exec` runs, empty sessions, and Zeron's own leftovers. `--all` shows them with a tag.
- **Titles:** imported chats keep the harness's own name for the session (Codex's thread name, or the pi session name). Chats without one stay untitled until you run `rename --untitled`.
- **Models:** if Zeron no longer offers a session's model, the chat uses Zeron's default model, and `import` prints a note.
- **Missing folders:** sessions whose folder is gone are tagged `[cwd missing]`. They still import, but can't resume until the folder exists again.
- **Re-import:** `import --replace` deletes the chat already linked to a session and imports it again, including anything added in Zeron since.
- **Export:** picks chats by `--chat` id prefixes and/or `--project` (name or folder); with neither, every chat. `--since` (a date or an age like `30d`) keeps chats active since then, `--until` (a date, included) keeps chats started by then, and `--limit N` keeps the N most recently active. Output is oldest first, with whole transcripts: user and assistant text plus one line per tool call. `--reasoning` adds the model's thinking, and `--no-tools` drops the tool lines. Without `-o`, everything goes to stdout. With `-o DIR`, it writes one file per chat.
- **Rename:** `rename` needs the `codex` CLI, logged in. Defaults: `--model gpt-6-luna --reasoning low`. Set `CODEX_BIN` or pass `--codex` if `codex` isn't on `PATH`.
