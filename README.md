# zeron-tools

Tools for a local [Zeron](https://github.com/zeronsh/zeron) profile (macOS, Apple silicon):

- `sync`: one-command upkeep (import, refresh, title, archive, clean up)
- `import`: pi sessions and Codex threads made outside Zeron, as chats that resume them
- `refresh`: show turns made outside Zeron in the Zeron chat
- `rename`: title chats with a small model through pi
- `archive`, `delete`: keep the sidebar short; remove unfinished chats and orphaned sessions
- `export`: chats as Markdown or JSONL, for example for an agent to review
- `connect`: use Zeron on all your Macs from a phone web app, with notifications

See `AGENTS.md` for how it works. `zeron-tools <command> --help` lists every option.

## Installation

```sh
curl -fsSL https://raw.githubusercontent.com/iyazerski/zeron-tools/main/install.sh | sh
```

It installs to `~/.local/bin`. Each release targets one Zeron version (`zeron-tools --version`); pin an older one with `ZERON_TOOLS_VERSION=v0.1.0`. To build from source, run `make install`.

## Usage

Commands that write need Zeron quit (Cmd+Q) and back up `docs.sqlite3` to `~/.zeron/zeron-tools-backups/` first. Reading commands and `--dry-run` work while Zeron runs.

```sh
zeron-tools sync --dry-run      # then without --dry-run, then: open -a Zeron
zeron-tools sessions codex      # sessions not in Zeron yet
zeron-tools import pi 01a0f866
zeron-tools export --project guid-service --since 30d -o ~/tmp/review
zeron-tools delete --unfinished --orphaned --dry-run
```

- `sync` deletes orphaned sessions, imports new sessions, adds outside turns, titles untitled chats, and archives all but the 10 latest chats per project. It skips sessions active in the last hour, in excluded folders, or whose folder is gone.
- A chat gets Zeron's default model if its session's model is no longer offered.
- `delete` also deletes the chat's pi session (to the Trash) or Codex thread (`codex delete`), unless `--zeron-only`.
- `rename` uses the `pi` CLI (default `--model gpt-6-luna`), 20 chats per call.

## Phone

```
phone ──HTTPS──▶ relay ◀──WebSocket── zeron-tools connect (each Mac) ──▶ Zeron
```

Each Mac dials out to a small relay, so no VPN or open ports are needed. You sign in with a passkey. The app lists the chats of all Macs, follows them live, sends messages, and notifies you when a chat finishes, needs you, or fails.

- **Relay:** `.github/workflows/deploy-relay.yml` deploys `relay/` to the k3s cluster (secret `KUBE_CONFIG_B64`). Before the first deploy, create the setup token, then open the setup link on the phone to create your passkey:

  ```sh
  kubectl create secret generic zeron-relay-secrets --from-literal=SETUP_TOKEN="$(openssl rand -hex 32)"
  echo "https://zeron.iyazerski.dev/#/setup/$(kubectl get secret zeron-relay-secrets -o jsonpath='{.data.SETUP_TOKEN}' | base64 -d)"
  ```

- **Each Mac:** in the app, open **⋯** → **Add a Mac** and run the command it shows. With `--install`, `connect` runs in the background from then on: it starts at login and logs to `~/Library/Logs/zeron-tools-connect.log` (`connect --uninstall` stops it). Zeron may be closed: the app shows that and can open it, or restart it when it hangs. The app shows one Mac at a time. Keep the Mac awake. Notifications wait until the Mac has been idle for 2 minutes (`--notify-always` skips that).
