# zeron-tools

Offline tools for a local [Zeron](https://github.com/zeronsh/zeron) profile:
- import pi sessions that were created outside Zeron
- retitle chats with a Codex model

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
zeron-tools sessions                       # pi sessions not in Zeron yet (works while Zeron runs)
zeron-tools import pi 01a0f866-31fb --dry-run

# Writes: quit Zeron (Cmd+Q) first. Each write backs up docs.sqlite3
# to ~/.zeron/zeron-tools-backups/<time>/ first.
zeron-tools import pi 01a0f866-31fb 01a0ecee-4cd5 --create-project
zeron-tools rename --untitled              # or --chat <id-prefix>..., or --all
open -a Zeron
```

`rename` needs the `codex` CLI, logged in. Defaults: `--model gpt-6-luna --reasoning low`. Set `CODEX_BIN` or pass `--codex` if `codex` isn't on `PATH`.

Imported chats keep the session's own pi name, if it has one. Otherwise they have no title until you run `rename --untitled`.
