# zeron-tools

A background service for a running [Zeron](https://github.com/zeronsh/zeron) (`connect`): sidebar upkeep (archive, titles) and a relay for using Zeron from a phone, plus `export`. Usage is in `README.md`.

## How it works

- Everything goes through the engine's loopback IPC (`ws://127.0.0.1:27654`, `--ipc-port`), the API Zeron's own window and MCP server use. No Zeron crates are linked. The engine rejects a handshake with an `Origin` header.
- Frames: one JSON object per text message. Client `{id, method, params}`, and `{id, cancel: true}` to stop a stream. Server `{id, ok}` or `{id, err}` for calls; `{id, item}`… then `{id, done: true}` for streams. Reading a list (`WatchChats`, `WatchSpaces`, `WatchSessions`, `WatchSidebarPreferences`, `WatchDocMessages`) takes a stream's first item (`Zeron::snapshot`). Writes are `Mutate` ops (`renameChat`, `setChatArchived`, `setChatConfig`, `createChat`) and `QueueCommand` (`run`, `steer`, `interrupt`, `respondInput`).
- Zeron's IPC changes additively between releases, so `src/zeron/types.rs` and `transcript.rs` read only the fields used, ignore unknown ones, map unknown enum values to `Other`, and skip parts they can't parse. Configs are written back with unknown fields kept (`ChatConfig.rest`).
- No version pin: `connect/engine.rs` probes `EngineReady` and, at startup and every few minutes, reads every list it uses (`check`). If one no longer parses or a method is unknown, Zeron counts as outdated: phone calls and upkeep stop until zeron-tools is updated.
- Upkeep (`connect/maintain.rs`), only while Zeron runs: `archive.rs` mirrors the sidebar's grouping (one project per repository, `Space::project_key`) and order (last activity), skips pinned and sectioned chats, and remembers what it archived (`zeron-tools-archived.json`) so a chat the user restores stays until new activity. `rename.rs` titles untitled, idle chats through pi.
- Phone: phone → HTTPS → relay ← outbound WebSocket ← `connect` on each Mac. `connect` runs only the tools the app needs (`connect/tools.rs` `has`, checked on the Mac: the raw IPC also runs terminals and writes files). They mirror Zeron's MCP tools of the same names (`crates/mcp/src/tools.rs`) and answer in the same shapes. It runs as a launch agent (`--install`) whether or not Zeron is open: it reports Zeron as running (with its version), closed, or outdated, refuses calls unless running, and can start or restart Zeron (`open_zeron`, `restart_zeron`: a fixed AppKit quit, then a force-quit if it hangs). It watches `WatchSessions` for finished, waiting, and failed chats, and the relay sends them as Web Push. Deploy relay changes before releasing a `connect` that sends new messages, and release a `connect` with new tools before deploying an app that calls them. Only one `connect` runs per data dir (`zeron-tools-connect.lock`). The relay keeps no chat data, only passkeys, hashed tokens, and push subscriptions.

## Layout

- `src/main.rs`: the CLI (`connect`, `export`).
- `src/zeron/`: the IPC client (`ipc.rs`), `Zeron` with typed reads and writes (`mod.rs`), lenient entity types (`types.rs`), and transcripts with a port of Zeron's `render_entries` (`transcript.rs`).
- `src/archive.rs`, `rename.rs`: the upkeep. `src/export.rs`: the export command.
- `src/connect/`: the relay link (`mod.rs`), Zeron's state (`engine.rs`), the upkeep schedule (`maintain.rs`), the launch agent (`agent.rs`), the phone's Zeron tools (`tools.rs`), the chat screen's state in one call (`chat.rs`), the composer's model and context (`composer.rs`), and the status watcher (`watch.rs`). `relay-proto/`: wire types shared with the relay.
- `relay/`: the relay crate, its app (`assets/`, plain JS styled after Zeron's iOS app), a `scratch`-image `Dockerfile`, and `k8s/` for the `hetzner` cluster.
- Tests: unit tests next to the code, against JSON shaped like the engine's.

## When Zeron changes

Most releases need nothing. If `connect` reports Zeron as outdated (its log names what failed to parse), or a feature misbehaves:

1. Get the Zeron source for the running version (`git ls-remote --tags https://github.com/zeronsh/zeron 'v*'`) and diff it against the last one that worked, mainly:
   - `crates/rpc/src/`: method names, `Mutate` ops, `QueueCommand` payloads, and frames
   - `crates/proto/src/entities.rs`: `Chat`, `ChatConfig`, `Space`, `Device`, `Session`, `SidebarPreferences`
   - `crates/doc/src/parts.rs`: message parts and tool calls
   - `crates/mcp/src/`: `transcript.rs` (`render_entries`, `tool_line`) and `tools.rs` (`summarize_chat`, `status_of`, `create_chat`, `send_message`), which `src/zeron/transcript.rs` and `src/connect/tools.rs` mirror
   - `crates/engine/src/titles.rs` (auto titles) and `crates/ui/src/`: sidebar order (`state.rs`, `shell/spaces.rs`) and the composer (`pickers.rs` `update_chat_config`)
2. Port the changes, keeping the types lenient.
3. Verify:
   - `cargo clippy --workspace --all-targets --locked -- -D warnings` and `cargo test --workspace --locked`.
   - With Zeron running: `zeron-tools export --limit 5`, then `connect --no-archive --no-rename` with a link, and check the log shows Zeron running.
   - Phone: start a local relay (`PUBLIC_URL=http://localhost:8790 BIND_ADDR=127.0.0.1:8790 DATA_DIR=<tmp> SETUP_TOKEN=dev cargo run -p zeron-relay`), open `/#/setup/dev` in Chrome with a virtual authenticator, add a Mac, run `connect`, and send a message.
4. Release: bump `version`, commit, and push the tag `vX.Y.Z`. `release.yml` builds and publishes the binary.
