# zeron-tools

Offline tools for a local Zeron profile: import pi sessions created outside Zeron, and retitle chats with a Codex model. Built against Zeron 0.2.113. See `AGENTS.md` for how it works and how to update it for a new Zeron release.

```sh
cargo install --path .

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
