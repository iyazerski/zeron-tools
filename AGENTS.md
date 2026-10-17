# zeron-tools

Offline maintenance CLI for a local [Zeron](https://github.com/zeronsh/zeron) profile. It assumes the user does not sync with Zeron's server (the `local` profile), so local writes are final.

- `sessions`: list harness sessions that no Zeron chat resumes yet.
- `import`: turn native harness sessions (pi today, Codex later) into Zeron chats. Each chat shows the history and stores `harnessSessionId`/`harnessSessionCwd`, so Zeron resumes the original session.
- `rename`: retitle chats with a Codex model (`codex exec` with structured output).

## How it works

- Zeron's own crates (`zeron-doc`, `zeron-sync`, `zeron-proto`) are git dependencies pinned to one release tag. The tool writes through their APIs and never hand-edits bytes.
- Zeron data lives in `~/.zeron/profiles/local/docs.sqlite3`:
  - `snapshots` table: one row per doc.
  - Registry doc `registry1`: chat, space (project), and device rows.
  - One Loro doc per chat: the transcript.
- Imports mirror the engine's "born chat2" path (`crates/engine/src/local_import.rs`): save the chat doc with cursor 0 and epoch 2, then upsert the row with `room_gen` 2. Tool outputs are dropped and tool inputs go through `sanitize_tool_call`, same as Zeron's render-parts policy.
- Zeron keeps the registry in memory and would overwrite offline writes. So writes take Zeron's `engine.lock` flock, which fails while Zeron runs, and back up `docs.sqlite3` to `~/.zeron/zeron-tools-backups/` first. Read-only commands and `--dry-run` work while Zeron runs.
- Writes are refused if the device row's `version` (what Zeron last booted) differs from `ZERON_VERSION`.

## Layout

- `src/zeron.rs`: profile access (lock, version check, backup, import, rename). It has no harness knowledge.
- `src/sources/`: one reader per harness, each producing a harness-neutral `Session`. `pi.rs` mirrors `crates/harness/src/pi/normalize.rs`.
- `src/rename.rs`: transcript excerpts, the Codex call, and output parsing.
- `src/main.rs`: the CLI.
- Tests: unit tests next to the code; fixtures in `tests/fixtures/`.

## Syncing with a new Zeron release

1. Find the latest tag: `git ls-remote --tags https://github.com/zeronsh/zeron 'v*'`.
2. Clone Zeron (or fetch both tags) and read `git diff <old>..<new> --stat` and the diffs for these paths:
   - `crates/proto/src/entities.rs`: `Chat`, `Space`, `ChatConfig`, `Device` fields
   - `crates/proto/src/` events: `AgentEvent`, `ToolCall`, `ToolDiff`, `HarnessId`, `ReasoningLevel`
   - `crates/doc/src/registry.rs`: `upsert_chat`, `upsert_space`, `rename_chat`, `read_*`, `set_local_only`, `to_bytes`/`from_bytes`, `REGISTRY_DOC_ID`
   - `crates/doc/src/schema.rs`: `SessionDoc`, `SessionMessageEntry`, `MessagePart`, `fold_event_into_parts`, `sanitize_tool_call`
   - `crates/sync/src/store.rs`: `DocsStore::open`, `load_snapshot`, `save_snapshot(_with_cursor)`, and the `snapshots` schema
   - `crates/engine/src/`:
     - `local_import.rs`: the born-chat shape
     - `chat2_host.rs`: `CHAT2_DOC_EPOCH`, room generation
     - `instance_lock.rs`: lock file name and flock
     - `profile.rs`: profile paths
     - `spaces.rs`: git probing of new spaces
   - `crates/harness/src/pi/`:
     - `normalize.rs`: the tool mapping to copy into `sources/pi.rs`
     - `sessions.rs`: resume by id, the `zeron-sessions` map
   - `crates/harness/src/codex/`: once a Codex source exists
3. Bump the tag in all `zeron-*` deps and `[package.metadata.zeron]` in `Cargo.toml`, and `ZERON_VERSION` in `src/zeron.rs`. Then run `cargo update -p zeron-doc -p zeron-sync -p zeron-proto`. Match `loro`/`rusqlite`/`tokio` versions to Zeron's workspace `Cargo.toml`.
4. Port relevant changes. Typical ones: new required fields, renamed APIs, changed doc epochs or room generations, and new pi tool mappings.
5. Verify:
   - `cargo clippy --all-targets` and `cargo test`
   - Smoke test against a copy of the real profile: copy `docs.sqlite3` into `<tmp>/profiles/local/`, then run `zeron-tools --data-dir <tmp> import pi <id>` and `rename --chat <id> --dry-run`.
   - Real import: quit Zeron, import one session, reopen Zeron, check that the chat shows and that a new message resumes the old context.

## Adding a source (e.g. Codex)

- Add `src/sources/<harness>.rs` with `session_files`, `summarize`, `parse` → `Session`, plus a `Harness` CLI variant.
- `Session.id` must be the id Zeron hands back to the harness for resume.
- Check against Zeron's harness code before relying on it. For Codex, Zeron resumes through app-server `thread/resume` (`crates/harness/src/codex/mod.rs`). Confirm which id and cwd rules that needs, and where Codex stores rollouts, before writing the reader.
- Reuse Zeron's normalizer mapping for that harness, so imported tool chips look native.
