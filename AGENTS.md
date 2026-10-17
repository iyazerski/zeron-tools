# zeron-tools

Offline maintenance CLI for a local [Zeron](https://github.com/zeronsh/zeron) profile. It assumes the user does not sync with Zeron's server (the `local` profile), so local writes are final.

- `sessions`: list harness sessions that no Zeron chat resumes yet.
- `import`: turn native pi sessions and Codex threads into Zeron chats. Each chat shows the history and stores `harnessSessionId`/`harnessSessionCwd`, so Zeron resumes the original session.
- `rename`: retitle chats with a Codex model (`codex exec` with structured output).
- `delete`: delete chats (picked, or every unfinished one) and the harness sessions they resume, plus orphaned sessions Zeron started.
- `export`: write chats (picked by id, project, date window, limit) as Markdown or JSONL, for people or agents to review.

## How it works

- Zeron's own crates (`zeron-doc`, `zeron-sync`, `zeron-proto`, `zeron-mcp`) are git dependencies pinned to one release tag. The tool writes through their APIs and never hand-edits bytes. `export` renders messages with `zeron_mcp::render_entries`, the renderer behind Zeron's `read_chat` MCP tool.
- Zeron data lives in `~/.zeron/profiles/local/docs.sqlite3`:
  - `snapshots` table: one row per doc.
  - Registry doc `registry1`: chat, space (project), and device rows.
  - One Loro doc per chat: the transcript.
- Imports mirror the engine's "born chat2" path (`crates/engine/src/local_import.rs`): save the chat doc with cursor 0 and epoch 2, then upsert the row with `room_gen` 2. Tool outputs are dropped and tool inputs go through `sanitize_tool_call`, same as Zeron's render-parts policy.
- Zeron keeps the registry in memory and would overwrite offline writes. So writes take Zeron's `engine.lock` flock, which fails while Zeron runs, and back up `docs.sqlite3` to `~/.zeron/zeron-tools-backups/` first. Read-only commands and `--dry-run` work while Zeron runs.
- Writes are refused if the device row's `version` (what Zeron last booted) differs from `ZERON_VERSION`.
- A session's model is kept only if Zeron's cached catalog for that harness (`~/.zeron/model-catalogs/<harness>/`, newest fetch) still lists it. Otherwise the chat gets no model and Zeron uses its default. Zeron passes the chat's model on resume, and a retired one breaks it.
- Sessions whose folder is gone still import (the history is readable), but never get a project, and can't resume until the folder exists.

## Layout

- `src/zeron.rs`: profile access (lock, version check, backup, import, rename). It has no harness knowledge.
- `src/sources/`: one reader per harness, each producing a harness-neutral `Session`. `Source` in `mod.rs` dispatches to them.
  - `pi.rs`: `~/.pi/agent/sessions/*/*.jsonl`, active branch via `parentId`. Mirrors `crates/harness/src/pi/normalize.rs`.
  - `codex.rs`: `~/.codex/sessions/**/rollout-*.jsonl` and `archived_sessions/`. A long thread continues in `rollout-<ts>-<id>_<segment>.jsonl` files; `threads()` groups them by the id in the name and they are read in order. It reads `event_msg`/`item_completed` items (Codex core `TurnItem`s: PascalCase types, snake_case fields) and mirrors `crates/harness/src/codex/normalize.rs::map_item`, which reads the app-server spelling of the same items. It also strips Codex Desktop's injected prompt context and takes names from `session_index.jsonl`. Subagent, `exec`, empty and Zeron-originated threads are hidden from `sessions`.
- `src/rename.rs`: transcript excerpts, the Codex call, and output parsing.
- `src/delete.rs`: the unfinished-chat rule: no assistant turn (continuations folded by `render_entries`) completed with text and no error. Harness-side deletion lives in the sources: `pi::delete_session` trashes the file like Pi's session picker and removes Zeron's `zeron-sessions/<sha256(id)>.json` record; `codex::delete_thread` runs `codex delete --force`. Deleting a chat mirrors Zeron's `DeleteChat` (row tombstone + `purge_chat` snapshot delete); Zeron itself never deletes harness sessions.
- `src/export.rs`: chat metadata plus rendered messages, Markdown/JSONL output, file names, and the `--since`/`--until` window. Headings inside message bodies are demoted two levels so `#` (chat) and `##` (message) stay unambiguous.
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
   - `crates/harness/src/codex/`:
     - `normalize.rs`: `map_item`, `file_change_call`, `ReasoningStream` separators, to mirror in `sources/codex.rs`
     - `mod.rs`: `thread/resume` by `harnessSessionId`, and which items become text (`contextCompaction`, `exitedReviewMode`)
   - `crates/engine/src/model_catalogs.rs`: the catalog cache format read by `Profile::model_catalog`
   - `crates/engine/src/rpc.rs` `MutateParams::DeleteChat` and `doc_host.rs` `purge_chat`: what deleting a chat removes (mirrored by `Profile::delete_chat`)
   - `crates/harness/src/pi/sessions.rs`: the `zeron-sessions` record that `pi::delete_session` removes
   - `crates/mcp/src/transcript.rs`: `render_entries`, `RenderOptions`, `RenderedMessage` (used by `export`)
3. Bump the tag in all `zeron-*` deps in `Cargo.toml` and in `zeron_version!` in `src/zeron.rs`. Then run `cargo update -p zeron-doc -p zeron-sync -p zeron-proto -p zeron-mcp`. Match `loro`/`rusqlite`/`tokio` versions to Zeron's workspace `Cargo.toml`.
4. Port relevant changes. Typical ones: new required fields, renamed APIs, changed doc epochs or room generations, and new tool mappings.
   - The rollout format belongs to Codex, not Zeron. If Codex was updated too, run `zeron-tools sessions codex` and `import codex <ids> --dry-run` on everything, and look for new item types: `rg -o --no-filename '"item":\{"type":"[A-Za-z]+"' ~/.codex/sessions | sort | uniq -c`.
5. Verify:
   - `cargo clippy --all-targets` and `cargo test`
   - Smoke test against a copy of the real profile: copy `docs.sqlite3` into `<tmp>/profiles/local/`, then run `zeron-tools --data-dir <tmp> import pi <id>`, `import codex <id>`, `rename --chat <id> --dry-run`, and `export --chat <id>`, and `delete --unfinished --orphaned --dry-run`. Copy `~/.zeron/model-catalogs` too, so the model check runs.
   - Real import: quit Zeron, import one session, reopen Zeron, check that the chat shows and that a new message resumes the old context.
6. Release: bump `version` in `Cargo.toml` (minor for a new Zeron release), commit, then push a matching tag (`git tag vX.Y.Z && git push origin vX.Y.Z`). `release.yml` checks that the tag matches the version, builds the macOS arm64 binary, and publishes it with a checksum. `install.sh` installs the latest release.

## Adding a source

- Add `src/sources/<harness>.rs` with `session_files`, `file_id`, `summarize`, `parse` → `Session`. Then add a `Source` variant and a `Harness` CLI variant.
- `Session.id` must be the id Zeron hands back to the harness for resume. Check Zeron's harness code for how it resumes (id, cwd, which home dir) before relying on it.
- Reuse Zeron's normalizer mapping for that harness, so imported tool chips look native.
- Mark sessions that aren't someone's conversation (subagents, scripted runs, Zeron's own leftovers) with `hidden_reason`.
