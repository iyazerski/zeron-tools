# zeron-tools

A background service for [Zeron](https://github.com/zeronsh/zeron) on macOS (Apple silicon). While Zeron runs, it:

- archives all but the 10 most recently active chats of each project (pinned chats and chats in sidebar sections stay),
- titles chats that stayed untitled, with a small model through pi,
- links Zeron to a relay, so you can use it on all your Macs from a phone web app, with notifications.

It talks to Zeron's engine over its local IPC and doesn't build against Zeron's code, so new Zeron releases don't need a new zeron-tools. `zeron-tools export` also writes chats as Markdown or JSONL, for example for an agent to review. See `AGENTS.md` for how it works. `zeron-tools <command> --help` lists every option.

## Installation

```sh
curl -fsSL https://raw.githubusercontent.com/iyazerski/zeron-tools/main/install.sh | sh
zeron-tools connect --install
```

It installs to `~/.local/bin`. Pin an older release with `ZERON_TOOLS_VERSION=v0.1.0`. To build from source, run `make install`.

`connect --install` runs the service in the background from then on: it starts at login, restarts after a crash, and logs to `~/Library/Logs/zeron-tools-connect.log`. `connect --uninstall` stops it. Run `--install` again to change its options.

## Usage

```sh
zeron-tools connect --install --keep 20 --title-model gpt-6-luna
zeron-tools connect --install --no-rename        # archive only
zeron-tools export --project guid-service --since 30d -o ~/tmp/review
```

- The upkeep runs a minute after Zeron opens, then every 15 minutes. Archived chats move to Zeron's Archived shelf and stay searchable and resumable. A chat you unarchive stays until it has new activity.
- Titling waits until a chat has been idle for 10 minutes and doesn't touch chats Zeron or you titled. It uses the `pi` CLI (default `--title-model gpt-6-luna`), 20 chats per call.
- `export` needs Zeron running.

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

- **Each Mac:** in the app, open **⋯** → **Add a Mac** and run the command it shows (`connect --install` with the relay and token; the link is saved). Zeron may be closed: the app shows that and can open it, or restart it when it hangs. The app shows one Mac at a time. Keep the Mac awake. Notifications wait until the Mac has been idle for 2 minutes (`--notify-always` skips that).
