//! Codex rollout files: `$CODEX_HOME/sessions/YYYY/MM/DD/rollout-<ts>-<id>.jsonl`
//! (archived threads move flat into `archived_sessions/`).
//!
//! Every line is `{timestamp, type, payload}`. The first is `session_meta`;
//! the transcript comes from `event_msg` / `item_completed` payloads, whose
//! `item` is Codex's core turn item (PascalCase `type`, snake_case fields).
//! Tool mapping mirrors Zeron's live Codex normalizer
//! (`crates/harness/src/codex/normalize.rs::map_item`), which reads the
//! app-server spelling of the same items, so imported chips look native.
//! Zeron resumes a chat by passing the thread id to `codex app-server`
//! `thread/resume`, which finds the rollout in the same `CODEX_HOME`.

use std::collections::HashMap;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use chrono::DateTime;
use serde_json::Value;
use zeron_doc::{MessagePart, MessageRole, fold_event_into_parts};
use zeron_proto::{AgentEvent, HarnessId, ToolCall};

use super::{Message, Session, SessionSummary};

/// Codex's home: `CODEX_HOME`, else `~/.codex` (the one Zeron runs Codex with).
pub fn codex_home() -> PathBuf {
    std::env::var_os("CODEX_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| crate::home().join(".codex"))
}

/// Every rollout under `sessions/` and `archived_sessions/`, oldest first.
pub fn session_files(home: &Path) -> Result<Vec<PathBuf>> {
    fn walk(dir: &Path, out: &mut Vec<PathBuf>) -> Result<()> {
        if !dir.is_dir() {
            return Ok(());
        }
        for entry in std::fs::read_dir(dir)? {
            let path = entry?.path();
            if path.is_dir() {
                walk(&path, out)?;
            } else if is_rollout(&path) {
                out.push(path);
            }
        }
        Ok(())
    }
    let mut files = vec![];
    walk(&home.join("sessions"), &mut files)?;
    walk(&home.join("archived_sessions"), &mut files)?;
    if files.is_empty() && !home.join("sessions").is_dir() {
        bail!("no Codex sessions in {}", home.display());
    }
    // `rollout-<ISO timestamp>-<id>`: name order is creation order.
    files.sort_by(|a, b| a.file_name().cmp(&b.file_name()));
    Ok(files)
}

fn is_rollout(path: &Path) -> bool {
    path.extension().is_some_and(|e| e == "jsonl")
        && path
            .file_name()
            .and_then(|n| n.to_str())
            .is_some_and(|n| n.starts_with("rollout-"))
}

/// The thread id a rollout file name ends with.
pub fn file_id(path: &Path) -> Option<&str> {
    let stem = path.file_stem()?.to_str()?;
    stem.get(stem.len().checked_sub(36)?..)
}

/// Codex's generated thread names (`session_index.jsonl`, last entry wins).
pub fn thread_names(home: &Path) -> HashMap<String, String> {
    let Ok(raw) = std::fs::read_to_string(home.join("session_index.jsonl")) else {
        return HashMap::new();
    };
    raw.lines()
        .filter_map(|l| serde_json::from_str::<Value>(l).ok())
        .filter_map(|e| {
            Some((
                s(&e, "id").to_owned(),
                e["thread_name"].as_str()?.to_owned(),
            ))
        })
        .filter(|(id, name)| !id.is_empty() && !name.trim().is_empty())
        .collect()
}

/// Why a thread is not someone's interactive conversation, if it isn't.
fn hidden_reason(meta: &Value) -> Option<String> {
    match &meta["source"] {
        Value::String(src) if src == "vscode" || src == "cli" => {}
        Value::String(src) => return Some(src.clone()),
        Value::Object(o) if o.contains_key("subagent") => return Some("subagent".into()),
        _ => return Some("unknown source".into()),
    }
    // Zeron's own threads (title runs, failed first attempts, deleted chats).
    (s(meta, "originator") == "zeron-native").then(|| "zeron leftover".into())
}

/// Read only what a listing needs (one pass, parsing only the lines it uses).
pub fn summarize(path: &Path, names: &HashMap<String, String>) -> Result<SessionSummary> {
    let reader = BufReader::new(
        std::fs::File::open(path).with_context(|| format!("opening {}", path.display()))?,
    );
    let mut lines = reader.lines();
    let meta: Value = serde_json::from_str(&lines.next().context("empty rollout")??)?;
    if meta["type"] != "session_meta" {
        bail!("not a Codex rollout (no session_meta first)");
    }
    let meta = &meta["payload"];
    let mut user_messages = 0;
    let mut first_user = String::new();
    for line in lines {
        let line = line?;
        // Cheap substring gate before parsing: rollouts run to hundreds of MB.
        if !line.contains(r#""item":{"type":"UserMessage""#) {
            continue;
        }
        user_messages += 1;
        if first_user.is_empty()
            && let Ok(e) = serde_json::from_str::<Value>(&line)
        {
            first_user = user_text(&e["payload"]["item"]);
        }
    }
    let id = s(meta, "id").to_owned();
    Ok(SessionSummary {
        cwd: s(meta, "cwd").into(),
        created_ms: ms(&meta["timestamp"]).unwrap_or(0),
        user_messages,
        first_user,
        name: names.get(&id).cloned(),
        hidden_reason: if user_messages == 0 {
            Some("empty".into())
        } else {
            hidden_reason(meta)
        },
        archived: is_archived(path),
        id,
    })
}

fn is_archived(path: &Path) -> bool {
    path.components()
        .any(|c| c.as_os_str() == "archived_sessions")
}

/// Parse a rollout into a [`Session`].
pub fn parse(path: &Path, names: &HashMap<String, String>) -> Result<Session> {
    let raw =
        std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    let mut lines = raw
        .lines()
        .filter_map(|l| serde_json::from_str::<Value>(l).ok());
    let meta = lines.next().filter(|m| m["type"] == "session_meta");
    let meta = &meta.context("not a Codex rollout (no session_meta first)")?["payload"];
    let id = s(meta, "id").to_owned();
    // Threads on other providers (deepseek, proxied anthropic/… ids) can't run
    // on Zeron's Codex as-is; leave the model unset so Zeron picks its default.
    let openai = matches!(s(meta, "model_provider"), "" | "openai");
    let mut session = Session {
        harness: HarnessId::Codex,
        cwd: s(meta, "cwd").into(),
        created_ms: ms(&meta["timestamp"]).unwrap_or(0),
        last_ms: 0,
        model: None,
        reasoning: None,
        name: names.get(&id).cloned(),
        archived: is_archived(path),
        messages: vec![],
        id,
    };
    let mut turn: Option<Message> = None;
    for line in lines {
        let p = &line["payload"];
        match (s(&line, "type"), s(p, "type")) {
            ("turn_context", _) => {
                let model = s(p, "model");
                session.model =
                    (openai && !model.is_empty() && !model.contains('/')).then(|| model.to_owned());
                session.reasoning = serde_json::from_value(p["effort"].clone()).ok();
            }
            ("event_msg", "turn_aborted") => {
                if let Some(t) = turn.as_mut() {
                    t.aborted = true;
                }
            }
            ("event_msg", "item_completed") => {
                let item = &p["item"];
                let started = p["started_at_ms"]
                    .as_i64()
                    .or_else(|| ms(&line["timestamp"]));
                let done = p["completed_at_ms"]
                    .as_i64()
                    .or(started)
                    .unwrap_or(session.last_ms);
                session.last_ms = session.last_ms.max(done);
                if s(item, "type") == "UserMessage" {
                    close_turn(&mut turn, &mut session.messages);
                    let text = user_text(item);
                    if !text.is_empty() {
                        session.messages.push(Message {
                            role: MessageRole::User,
                            parts: vec![MessagePart::Text {
                                id: "t0".into(),
                                text,
                            }],
                            created_ms: started.unwrap_or(done),
                            duration_ms: None,
                            aborted: false,
                        });
                    }
                    continue;
                }
                let events = item_events(item);
                if events.is_empty() {
                    continue;
                }
                let t = turn.get_or_insert_with(|| Message {
                    role: MessageRole::Assistant,
                    parts: vec![],
                    created_ms: started.unwrap_or(done),
                    duration_ms: Some(0),
                    aborted: false,
                });
                t.duration_ms = Some((done - t.created_ms).max(0));
                for event in events {
                    // Zeron's ReasoningStream: separate distinct reasoning
                    // parts so adjacent headings don't run together.
                    if let AgentEvent::ReasoningDelta { text } = &event
                        && let Some(MessagePart::Reasoning { text: prev, .. }) = t.parts.last()
                    {
                        let gap = 2usize.saturating_sub(
                            prev.chars().rev().take_while(|&c| c == '\n').count()
                                + text.chars().take_while(|&c| c == '\n').count(),
                        );
                        if gap > 0 {
                            fold_event_into_parts(
                                &mut t.parts,
                                &AgentEvent::ReasoningDelta {
                                    text: "\n".repeat(gap),
                                },
                            );
                        }
                    }
                    fold_event_into_parts(&mut t.parts, &event);
                }
            }
            _ => {}
        }
    }
    close_turn(&mut turn, &mut session.messages);
    if session.messages.is_empty() {
        bail!("{} has no messages", path.display());
    }
    Ok(session)
}

fn close_turn(turn: &mut Option<Message>, out: &mut Vec<Message>) {
    if let Some(t) = turn.take().filter(|t| !t.parts.is_empty()) {
        out.push(t);
    }
}

/// Events Zeron's normalizer would emit for one completed item.
fn item_events(item: &Value) -> Vec<AgentEvent> {
    let id = s(item, "id").to_owned();
    let status = s(item, "status");
    let tool = |call: ToolCall, is_error: bool| {
        vec![
            AgentEvent::ToolCall {
                id: id.clone(),
                call,
            },
            AgentEvent::ToolResult {
                id: id.clone(),
                is_error,
                output: None,
                diff: None,
            },
        ]
    };
    let text = |text: String| {
        if text.trim().is_empty() {
            return vec![];
        }
        // Zeron closes every agent message with a paragraph break.
        vec![
            AgentEvent::TextDelta { text },
            AgentEvent::TextDelta {
                text: "\n\n".into(),
            },
        ]
    };
    match s(item, "type") {
        "AgentMessage" => text(
            item["content"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|c| c["text"].as_str())
                .collect::<Vec<_>>()
                .join(""),
        ),
        // Zeron drops plan items; keep the text since it is what the user read.
        "Plan" => text(s(item, "text").to_owned()),
        "Reasoning" => item["summary_text"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
            .filter(|t| !t.is_empty())
            .map(|t| AgentEvent::ReasoningDelta { text: t.to_owned() })
            .collect(),
        "CommandExecution" => {
            let exit = item["exit_code"].as_i64().unwrap_or(0);
            tool(
                ToolCall::Exec {
                    command: command(&item["command"]),
                },
                status == "failed" || exit != 0,
            )
        }
        "FileChange" => {
            let changes: Vec<(String, &str)> = item["changes"]
                .as_object()
                .into_iter()
                .flatten()
                .map(|(path, c)| {
                    let kind = match s(c, "type") {
                        k @ ("add" | "delete" | "update") => k,
                        _ => "update",
                    };
                    (path.clone(), kind)
                })
                .collect();
            tool(
                file_change_call(&changes),
                matches!(status, "failed" | "declined"),
            )
        }
        "McpToolCall" => tool(
            ToolCall::Mcp {
                server: s(item, "server").into(),
                tool: s(item, "tool").into(),
                input: Some(item["arguments"].clone()).filter(|v| !v.is_null()),
            },
            status == "failed",
        ),
        "WebSearch" => tool(
            ToolCall::WebSearch {
                query: search_query(item),
            },
            false,
        ),
        "Extension" => match s(item, "kind") {
            "web.search" => tool(
                ToolCall::WebSearch {
                    query: search_query(item),
                },
                false,
            ),
            // Zeron renders the saved image too; that needs the file copied
            // into its uploads root, so imports keep only the chip.
            "image_gen.generation" => tool(
                ToolCall::Unknown {
                    name: "Generate image".into(),
                    input: None,
                },
                false,
            ),
            _ => vec![],
        },
        "SubAgentActivity" if matches!(s(item, "kind"), "started" | "spawned") => {
            let name = s(item, "agent_path")
                .rsplit('/')
                .find(|seg| !seg.is_empty())
                .map(|leaf| format!("Agent: {leaf}"))
                .unwrap_or_else(|| "Agent".into());
            tool(
                ToolCall::Unknown {
                    name,
                    input: Some(item.clone()),
                },
                false,
            )
        }
        "ContextCompaction" => vec![AgentEvent::TextDelta {
            text: "Context compacted.".into(),
        }],
        "ExitedReviewMode" => item["review"]
            .as_str()
            .map(|r| vec![AgentEvent::TextDelta { text: r.into() }])
            .unwrap_or_default(),
        // Zeron shows none of these: image views, app tools, hook prompts.
        _ => vec![],
    }
}

/// Zeron's `file_change_call`: a lone add is a write, a lone update an edit,
/// anything else a patch.
fn file_change_call(changes: &[(String, &str)]) -> ToolCall {
    match changes {
        [(path, "add")] => ToolCall::WriteFile {
            path: path.clone(),
            content: None,
        },
        [(path, "update")] => ToolCall::EditFile {
            path: path.clone(),
            old_string: None,
            new_string: None,
        },
        [(path, _)] => ToolCall::ApplyPatch {
            path: Some(path.clone()),
        },
        _ => ToolCall::ApplyPatch { path: None },
    }
}

/// The app server hands Zeron a command string; rollouts keep argv. Show the
/// script of a `<shell> -lc <script>` wrapper, else the joined argv.
fn command(argv: &Value) -> String {
    let args: Vec<&str> = argv
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .collect();
    match args.as_slice() {
        [_, flag, script] if matches!(*flag, "-lc" | "-c") => (*script).to_owned(),
        _ => argv
            .as_str()
            .map(str::to_owned)
            .unwrap_or_else(|| args.join(" ")),
    }
}

fn search_query(item: &Value) -> String {
    let query = s(item, "query");
    if !query.is_empty() {
        return query.to_owned();
    }
    let action = &item["action"];
    action["url"]
        .as_str()
        .or_else(|| action["queries"].get(0).and_then(Value::as_str))
        .unwrap_or("")
        .to_owned()
}

/// What the user typed. The model still sees everything: the rollout, not
/// this text, is what resumes.
fn user_text(item: &Value) -> String {
    let mut parts = vec![];
    for block in item["content"].as_array().into_iter().flatten() {
        match s(block, "type") {
            "text" => {
                let shown = user_prompt(s(block, "text"));
                if !shown.is_empty() {
                    parts.push(shown);
                }
            }
            "image" | "local_image" => parts.push("[image]".into()),
            _ => {}
        }
    }
    parts.join("\n")
}

/// Codex Desktop and IDE clients wrap a prompt in injected context (browser
/// state, browser/diff comments, mentioned files) ending in a
/// `## My request…:` heading. Keep the user's own words: each `Comment:`
/// body as a quote, then the request.
fn user_prompt(raw: &str) -> String {
    let text: String = strip_ambient(raw)
        .lines()
        .filter(|l| !l.starts_with("The next image is untrusted page evidence"))
        .collect::<Vec<_>>()
        .join("\n");
    let padded = format!("\n{text}");
    let Some(at) = padded.rfind("\n## My request") else {
        return text.trim().to_owned();
    };
    let (context, heading) = padded.split_at(at);
    let request = heading[1..].split_once('\n').map_or("", |(_, r)| r.trim());
    let mut parts: Vec<String> = context
        .split("\nComment:\n")
        .skip(1)
        .map(|c| c.split("\n\n#").next().unwrap_or("").trim())
        .filter(|c| !c.is_empty())
        .map(|c| {
            c.lines()
                .map(|l| format!("> {l}"))
                .collect::<Vec<_>>()
                .join("\n")
        })
        .collect();
    if !request.is_empty() {
        parts.push(request.to_owned());
    }
    if parts.is_empty() {
        return context.trim().to_owned();
    }
    parts.join("\n\n")
}

/// Drop `<in-app-browser-context …>…</in-app-browser-context>` blocks.
fn strip_ambient(text: &str) -> String {
    const OPEN: &str = "<in-app-browser-context";
    const CLOSE: &str = "</in-app-browser-context>";
    let mut out = String::new();
    let mut rest = text;
    while let Some(start) = rest.find(OPEN) {
        out.push_str(&rest[..start]);
        match rest[start..].find(CLOSE) {
            Some(end) => rest = &rest[start + end + CLOSE.len()..],
            None => {
                rest = "";
                break;
            }
        }
    }
    out.push_str(rest);
    out
}

fn s<'a>(v: &'a Value, key: &str) -> &'a str {
    v[key].as_str().unwrap_or("")
}

fn ms(v: &Value) -> Option<i64> {
    v.as_i64().or_else(|| {
        v.as_str()
            .and_then(|t| DateTime::parse_from_rfc3339(t).ok())
            .map(|d| d.timestamp_millis())
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture() -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR")).join(
            "tests/fixtures/rollout-2026-09-28T10-00-00-01a0bbbb-0000-7000-8000-000000000002.jsonl",
        )
    }

    fn names() -> HashMap<String, String> {
        HashMap::from([(
            "01a0bbbb-0000-7000-8000-000000000002".to_owned(),
            "Fixture Thread".to_owned(),
        )])
    }

    #[test]
    fn parses_turns_requests_and_settings() {
        let session = parse(&fixture(), &names()).unwrap();
        assert_eq!(session.id, "01a0bbbb-0000-7000-8000-000000000002");
        assert_eq!(file_id(&fixture()), Some(session.id.as_str()));
        assert_eq!(session.name.as_deref(), Some("Fixture Thread"));
        assert_eq!(session.model.as_deref(), Some("gpt-6-sol"));
        assert_eq!(session.reasoning, Some(zeron_proto::ReasoningLevel::High));
        let roles: Vec<_> = session.messages.iter().map(|m| m.role).collect();
        assert_eq!(
            roles,
            [
                MessageRole::User,
                MessageRole::Assistant,
                MessageRole::User,
                MessageRole::Assistant
            ]
        );
        // Injected Desktop context is stripped down to the request.
        assert_eq!(session.messages[0].text(), "Fix the login bug\n[image]");
        assert_eq!(session.messages[2].text(), "Thanks");
        assert!(session.messages[3].aborted);
    }

    #[test]
    fn items_fold_into_native_chips() {
        let session = parse(&fixture(), &names()).unwrap();
        let turn = &session.messages[1];
        let MessagePart::Reasoning { text, .. } = &turn.parts[0] else {
            panic!("expected reasoning, got {:?}", turn.parts[0]);
        };
        assert_eq!(text, "**Plan**\n\n**Look**");
        let calls: Vec<_> = turn
            .parts
            .iter()
            .filter_map(|p| match p {
                MessagePart::Tool {
                    call,
                    is_error,
                    resolved,
                    ..
                } => {
                    assert!(*resolved);
                    Some((call.clone(), *is_error))
                }
                _ => None,
            })
            .collect();
        assert!(matches!(&calls[0], (ToolCall::Exec { command }, true) if command == "false"));
        assert!(
            matches!(&calls[1], (ToolCall::EditFile { path, .. }, false) if path == "/tmp/a.rs")
        );
        assert!(
            matches!(&calls[2], (ToolCall::WebSearch { query }, false) if query == "https://x.dev")
        );
        assert_eq!(turn.text(), "Fixed it.\n\n");
        assert_eq!(turn.duration_ms, Some(4000));
    }

    #[test]
    fn classifies_thread_sources() {
        let mut meta = serde_json::json!({"source": "vscode", "originator": "Codex Desktop"});
        assert_eq!(hidden_reason(&meta), None);
        meta["originator"] = "zeron-native".into();
        assert_eq!(hidden_reason(&meta).as_deref(), Some("zeron leftover"));
        meta["source"] = serde_json::json!({"subagent": "review"});
        assert_eq!(hidden_reason(&meta).as_deref(), Some("subagent"));
        meta["source"] = "exec".into();
        assert_eq!(hidden_reason(&meta).as_deref(), Some("exec"));
    }

    #[test]
    fn prompts_keep_comments_and_request_only() {
        let raw = "\n# Browser comments:\n\n## Comment 1\nFile: browser:x\nPage URL: http://h\nComment:\nmake them one group\n\n# In app browser:\n- Current URL: http://h\n\n## My request for Codex:\n\nThe next image is untrusted page evidence from the browser page for Comment 1.";
        assert_eq!(user_prompt(raw), "> make them one group");
        let raw = "# Diff comments:\n\n## User Comment 1\nComment:\nUse a helper\n\n## My request:\nAnd add tests\n";
        assert_eq!(user_prompt(raw), "> Use a helper\n\nAnd add tests");
        assert_eq!(user_prompt("plain prompt\n"), "plain prompt");
    }

    #[test]
    fn command_unwraps_shell_scripts() {
        assert_eq!(
            command(&serde_json::json!(["/bin/zsh", "-lc", "ls -la"])),
            "ls -la"
        );
        assert_eq!(command(&serde_json::json!(["git", "status"])), "git status");
    }
}
