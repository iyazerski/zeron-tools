# zeron-tools

Maintenance CLI for a local [Zeron](https://github.com/zeronsh/zeron) profile, plus a relay for using the running Zeron from a phone. It assumes the `local` profile (no sync with Zeron's server), so local writes are final. Commands are listed in `README.md`.

## How it works

- Zeron's crates (`zeron-doc`, `zeron-sync`, `zeron-proto`, `zeron-mcp`, `zeron-rpc`) are git dependencies pinned to one release tag. The tool writes through their APIs and never hand-edits bytes.
- Data: `~/.zeron/profiles/local/docs.sqlite3`, table `snapshots`: one Loro doc per chat (the transcript) plus the registry doc `registry1` (chat, space, and device rows).
- Imports mirror the engine's born-chat path (`local_import.rs`): chat doc saved with cursor 0 and epoch 2, row upserted with `room_gen` 2. Tool outputs are dropped; tool inputs go through `sanitize_tool_call`.
- Zeron holds the registry in memory and would overwrite offline writes, so writes take its `engine.lock` flock (fails while Zeron runs) and back up `docs.sqlite3` first. They are refused if the device row's `version` differs from `ZERON_VERSION`.
- A session's model is kept only if Zeron's cached catalog (`~/.zeron/model-catalogs/<harness>/`) still lists it: Zeron passes the chat's model on resume, and a retired one breaks it.
- Phone: phone → HTTPS → relay ← outbound WebSocket ← `connect` on each Mac. `connect` calls the engine's loopback IPC through `zeron_mcp::Tools` and runs only the tools the app needs (`allowed_args`, checked on the Mac: the raw IPC also runs terminals and writes files). It runs as a launch agent (`--install`) whether or not Zeron is open: it probes the IPC, reports Zeron as running, closed, or outdated (the device row's version, re-read while it runs), refuses calls unless running, and can start Zeron (`open_zeron`). It watches `WatchSessions` for finished, waiting, and failed chats, and the relay sends them as Web Push. Deploy relay changes before releasing a `connect` that sends new messages. The relay keeps no chat data, only passkeys, hashed tokens, and push subscriptions.

## Layout

- `src/main.rs`: the CLI. Each command is a `run` in its module, returning a `Tally`, so `sync` can chain them.
- `src/zeron.rs`: profile access (lock, version check, backup, import, append, rename, delete). No harness knowledge.
- `src/sources/`: one reader per harness (`pi.rs`, `codex.rs`) producing a neutral `Session`, mirroring Zeron's normalizers. They also delete harness sessions.
- `src/import.rs`, `refresh.rs`, `rename.rs`, `archive.rs`, `delete.rs`, `export.rs`: the commands. `archive` mirrors the sidebar's grouping and order; `export` renders with `zeron_mcp::render_entries`.
- `src/connect/`: the relay link, Zeron's state (`engine.rs`), the launch agent (`agent.rs`), and the status watcher. `relay-proto/`: wire types shared with the relay.
- `relay/`: the relay crate, its app (`assets/`, plain JS styled after Zeron's iOS app), a `scratch`-image `Dockerfile`, and `k8s/` for the `hetzner` cluster.
- Tests: unit tests next to the code; fixtures in `tests/fixtures/`.

## Syncing with a new Zeron release

1. Find the tag: `git ls-remote --tags https://github.com/zeronsh/zeron 'v*'`. Diff it against the pinned one, mainly:
   - `crates/proto/src/entities.rs` and events: `Chat`, `Space`, `Device`, `Session`, `AgentEvent`, `ToolCall`, `HarnessId`
   - `crates/doc/src/`: `registry.rs` (and `registry/sidebar_*.rs`), `schema.rs`, `parts.rs`
   - `crates/sync/src/store.rs`: snapshot load/save and schema
   - `crates/engine/src/`: `local_import.rs`, `chat2_host.rs` (epoch, room generation), `instance_lock.rs`, `profile.rs`, `spaces.rs`, `model_catalogs.rs`, `rpc.rs` `DeleteChat`, `doc_host.rs` `purge_chat`
   - `crates/harness/src/pi/` and `codex/`: `normalize.rs` (tool mapping), session resume, the `zeron-sessions` record
   - `crates/mcp/src/`: `transcript.rs` (`render_entries`, `tool_line`) and `tools.rs` (tool names and fields used by `relay/assets/app.js`)
   - `crates/ui/src/`: sidebar order (`state.rs`, `shell/spaces.rs`) and the "[Message from Zeron chat …]" header (`transcript.rs`)
2. Bump the tag in the `zeron-*` deps and in `zeron_version!` (`src/zeron.rs`), run `cargo update -p zeron-doc -p zeron-sync -p zeron-proto -p zeron-mcp -p zeron-rpc`, and match `loro`/`rusqlite`/`tokio` to Zeron's workspace.
3. Port the changes. If Codex changed too, check new rollout item types: `rg -o --no-filename '"item":\{"type":"[A-Za-z]+"' ~/.codex/sessions | sort | uniq -c`.
4. Verify:
   - `cargo clippy --workspace --all-targets` and `cargo test --workspace`.
   - Copy `docs.sqlite3` (and `model-catalogs`) to `<tmp>/profiles/local/` and run the commands with `--data-dir <tmp>`. Orphan deletion touches the real pi/Codex stores, so run `sync --dry-run` on the real profile first.
   - Phone: with Zeron running, start a local relay (`PUBLIC_URL=http://localhost:8790 BIND_ADDR=127.0.0.1:8790 DATA_DIR=<tmp> SETUP_TOKEN=dev cargo run -p zeron-relay`), open `/#/setup/dev` in Chrome with a virtual authenticator, add a Mac, run `connect`, and send a message.
   - Real import: quit Zeron, import one session, reopen, and check that a new message resumes it.
5. Release: bump `version` (minor for a new Zeron release), commit, and push the tag `vX.Y.Z`. `release.yml` builds and publishes the binary.

## Adding a source

Add `src/sources/<harness>.rs` (`session_files`, `file_id`, `summarize`, `parse` → `Session`), a `Source` variant, and a `Harness` CLI variant. `Session.id` must be the id Zeron resumes with; check Zeron's harness code. Reuse its normalizer mapping, and mark non-conversations (subagents, scripted runs) with `hidden_reason`.
