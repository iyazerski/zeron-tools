//! Pi (pi.dev) session files: `~/.pi/agent/sessions/<cwd-slug>/<ts>_<id>.jsonl`.
//!
//! A file is a tree of entries linked by `parentId`; the conversation shown to
//! the user is the path from the last entry back to the root. Tool mapping
//! mirrors Zeron's live Pi normalizer (`crates/harness/src/pi/normalize.rs`),
//! so imported chips look like ones Zeron recorded itself.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use chrono::DateTime;
use serde_json::Value;
use zeron_doc::{MessagePart, MessageRole, fold_event_into_parts};
use zeron_proto::{AgentEvent, HarnessId, ToolCall, ToolDiff};

use super::{Message, Session, SessionSummary};

/// Pi's settings directory: `PI_CODING_AGENT_DIR`, else `~/.pi/agent` (same rule as Zeron).
pub fn agent_dir() -> PathBuf {
    std::env::var_os("PI_CODING_AGENT_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| crate::home().join(".pi/agent"))
}

/// Whether Zeron itself started this session: its Pi store records every
/// session it creates at `zeron-sessions/<sha256(id)>.json`
/// (`crates/harness/src/pi/sessions.rs::key`). Such a session with no chat
/// is a leftover (a failed first attempt or a deleted chat), not foreign work.
pub fn started_by_zeron(agent_dir: &Path, id: &str) -> bool {
    use sha2::{Digest, Sha256};
    agent_dir
        .join("zeron-sessions")
        .join(format!("{:x}.json", Sha256::digest(id.as_bytes())))
        .is_file()
}

/// Every session file under `<agent_dir>/sessions`, oldest first.
pub fn session_files(agent_dir: &Path) -> Result<Vec<PathBuf>> {
    let mut files = vec![];
    let root = agent_dir.join("sessions");
    for dir in std::fs::read_dir(&root).with_context(|| format!("reading {}", root.display()))? {
        let dir = dir?.path();
        if !dir.is_dir() {
            continue;
        }
        for file in std::fs::read_dir(&dir)? {
            let file = file?.path();
            if file.extension().is_some_and(|e| e == "jsonl") {
                files.push(file);
            }
        }
    }
    // File names start with an ISO timestamp, so name order is creation order.
    files.sort_by(|a, b| a.file_name().cmp(&b.file_name()));
    Ok(files)
}

/// The session id a file name (`<timestamp>_<id>.jsonl`) ends with.
pub fn file_id(path: &Path) -> Option<&str> {
    path.file_stem()?.to_str()?.rsplit('_').next()
}

/// Read only what a listing needs.
pub fn summarize(agent_dir: &Path, path: &Path) -> Result<SessionSummary> {
    let entries = read_entries(path)?;
    let header = header(&entries)?;
    let users: Vec<&Value> = entries
        .iter()
        .filter(|e| e["type"] == "message" && e["message"]["role"] == "user")
        .collect();
    let id = s(header, "id");
    let hidden_reason = if users.is_empty() {
        Some("empty".into())
    } else {
        started_by_zeron(agent_dir, id).then(|| "zeron leftover".into())
    };
    Ok(SessionSummary {
        id: id.into(),
        cwd: s(header, "cwd").into(),
        created_ms: ms(&header["timestamp"]).unwrap_or(0),
        user_messages: users.len(),
        first_user: users
            .first()
            .map(|e| user_text(&e["message"]))
            .unwrap_or_default(),
        name: entries
            .iter()
            .rev()
            .find(|e| e["type"] == "session_info")
            .and_then(|e| e["name"].as_str())
            .map(str::to_owned),
        hidden_reason,
        archived: false,
    })
}

/// Parse a session file's active branch into a [`Session`].
pub fn parse(path: &Path) -> Result<Session> {
    let entries = read_entries(path)?;
    let header = header(&entries)?;
    let mut session = Session {
        harness: HarnessId::Pi,
        id: s(header, "id").into(),
        cwd: s(header, "cwd").into(),
        created_ms: ms(&header["timestamp"]).unwrap_or(0),
        last_ms: 0,
        model: None,
        reasoning: None,
        name: None,
        archived: false,
        messages: vec![],
    };
    let mut tools: HashMap<String, (String, Value)> = HashMap::new();
    let mut turn: Option<Message> = None;
    for entry in active_branch(&entries) {
        let ts = ms(&entry["timestamp"]).unwrap_or(session.last_ms);
        match s(entry, "type") {
            "model_change" => {
                session.model = Some(format!("{}/{}", s(entry, "provider"), s(entry, "modelId")))
            }
            "thinking_level_change" => {
                session.reasoning = serde_json::from_value(entry["thinkingLevel"].clone()).ok()
            }
            "session_info" if entry["name"].is_string() => {
                session.name = Some(s(entry, "name").into())
            }
            "message" => {
                let m = &entry["message"];
                let at = ms(&m["timestamp"]).unwrap_or(ts);
                session.last_ms = session.last_ms.max(at);
                match s(m, "role") {
                    "user" => {
                        close_turn(&mut turn, &mut session.messages);
                        session.messages.push(Message {
                            role: MessageRole::User,
                            parts: vec![MessagePart::Text {
                                id: "t0".into(),
                                text: user_text(m),
                            }],
                            created_ms: at,
                            duration_ms: None,
                            aborted: false,
                        });
                    }
                    "assistant" => {
                        let t = turn.get_or_insert_with(|| Message {
                            role: MessageRole::Assistant,
                            parts: vec![],
                            created_ms: at,
                            duration_ms: Some(0),
                            aborted: false,
                        });
                        t.duration_ms = Some((at - t.created_ms).max(0));
                        if let Some(model) = m["model"].as_str() {
                            session.model = Some(format!("{}/{}", s(m, "provider"), model));
                        }
                        for block in m["content"].as_array().into_iter().flatten() {
                            let event = match s(block, "type") {
                                "text" if !s(block, "text").is_empty() => AgentEvent::TextDelta {
                                    text: s(block, "text").into(),
                                },
                                "thinking" if !s(block, "thinking").is_empty() => {
                                    AgentEvent::ReasoningDelta {
                                        text: s(block, "thinking").into(),
                                    }
                                }
                                "toolCall" => {
                                    let id = s(block, "id").to_owned();
                                    let args = block["arguments"].clone();
                                    let call = tool(s(block, "name"), &args);
                                    tools.insert(id.clone(), (s(block, "name").into(), args));
                                    AgentEvent::ToolCall { id, call }
                                }
                                _ => continue,
                            };
                            fold_event_into_parts(&mut t.parts, &event);
                        }
                        match s(m, "stopReason") {
                            "aborted" => t.aborted = true,
                            "error" => fold_event_into_parts(
                                &mut t.parts,
                                &AgentEvent::Error {
                                    message: m["errorMessage"]
                                        .as_str()
                                        .unwrap_or("Pi provider failed")
                                        .into(),
                                },
                            ),
                            _ => {}
                        }
                    }
                    "toolResult" => {
                        if let Some(t) = turn.as_mut() {
                            t.duration_ms = Some((at - t.created_ms).max(0));
                            let id = s(m, "toolCallId").to_owned();
                            let is_error = m["isError"].as_bool().unwrap_or(false);
                            let diff = tools
                                .get(&id)
                                .filter(|_| !is_error)
                                .and_then(|(name, args)| diff(name, args));
                            // Outputs never enter Zeron docs; only error state and diff stats do.
                            fold_event_into_parts(
                                &mut t.parts,
                                &AgentEvent::ToolResult {
                                    id,
                                    is_error,
                                    output: None,
                                    diff,
                                },
                            );
                        }
                    }
                    _ => {}
                }
            }
            _ => {}
        }
    }
    close_turn(&mut turn, &mut session.messages);
    if session.messages.is_empty() {
        bail!("{} has no messages on its active branch", path.display());
    }
    Ok(session)
}

fn close_turn(turn: &mut Option<Message>, out: &mut Vec<Message>) {
    if let Some(t) = turn.take().filter(|t| !t.parts.is_empty()) {
        out.push(t);
    }
}

fn read_entries(path: &Path) -> Result<Vec<Value>> {
    let raw =
        std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    Ok(raw
        .lines()
        .filter_map(|l| serde_json::from_str(l).ok())
        .collect())
}

fn header(entries: &[Value]) -> Result<&Value> {
    entries
        .iter()
        .find(|e| e["type"] == "session")
        .context("not a Pi session file (no session header)")
}

/// The branch the user last worked on: parents of the last entry, root first.
fn active_branch(entries: &[Value]) -> Vec<&Value> {
    let by_id: HashMap<&str, &Value> = entries
        .iter()
        .filter_map(|e| e["id"].as_str().map(|id| (id, e)))
        .collect();
    let mut path = vec![];
    let mut cur = entries
        .iter()
        .rev()
        .find(|e| e["id"].is_string() && e["type"] != "session");
    while let Some(e) = cur {
        path.push(e);
        cur = e["parentId"].as_str().and_then(|p| by_id.get(p).copied());
    }
    path.reverse();
    path
}

fn user_text(message: &Value) -> String {
    match &message["content"] {
        Value::String(text) => text.clone(),
        Value::Array(items) => items
            .iter()
            .filter_map(|i| match s(i, "type") {
                "text" => Some(s(i, "text").to_owned()),
                // Pasted images are base64 in the session; Zeron would need them
                // copied into its uploads root. Keep a visible marker instead.
                "image" => Some("[image]".to_owned()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n"),
        _ => String::new(),
    }
}

/// Zeron's Pi tool mapping (`normalize.rs::tool`).
fn tool(name: &str, args: &Value) -> ToolCall {
    match name {
        "bash" => ToolCall::Exec {
            command: s(args, "command").into(),
        },
        "read" => ToolCall::ReadFile {
            path: s(args, "path").into(),
        },
        "write" => ToolCall::WriteFile {
            path: s(args, "path").into(),
            content: args["content"].as_str().map(str::to_owned),
        },
        "edit" => ToolCall::EditFile {
            path: s(args, "path").into(),
            // Zeron's sanitizer drops these anyway; the diff carries the change.
            old_string: None,
            new_string: None,
        },
        "grep" => ToolCall::Search {
            pattern: s(args, "pattern").into(),
            path: args["path"].as_str().map(str::to_owned),
        },
        "find" => ToolCall::Glob {
            pattern: s(args, "pattern").into(),
        },
        n if n.starts_with("zeron_") => ToolCall::Mcp {
            server: "zeron".into(),
            tool: n["zeron_".len()..].into(),
            input: Some(args.clone()),
        },
        _ => ToolCall::Unknown {
            name: name.into(),
            input: Some(args.clone()),
        },
    }
}

/// Diff for a successful edit/write, so the chip carries +/- stats. Pi's
/// current `edit` takes `edits[]`; older sessions used `oldText`/`newText`.
fn diff(name: &str, args: &Value) -> Option<ToolDiff> {
    let path = s(args, "path").to_owned();
    match name {
        "edit" => {
            let (old, new) = match args["edits"].as_array() {
                Some(edits) => (
                    edits
                        .iter()
                        .map(|e| s(e, "oldText"))
                        .collect::<Vec<_>>()
                        .join("\n"),
                    edits
                        .iter()
                        .map(|e| s(e, "newText"))
                        .collect::<Vec<_>>()
                        .join("\n"),
                ),
                None => (s(args, "oldText").into(), s(args, "newText").into()),
            };
            Some(ToolDiff {
                path,
                old_text: Some(old),
                new_text: new,
            })
        }
        "write" => Some(ToolDiff {
            path,
            old_text: None,
            new_text: s(args, "content").into(),
        }),
        _ => None,
    }
}

fn s<'a>(v: &'a Value, key: &str) -> &'a str {
    v[key].as_str().unwrap_or("")
}

/// Pi stores epoch millis on messages and RFC 3339 strings on entries.
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
    use zeron_doc::MessagePart;

    fn fixture() -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/pi-session.jsonl")
    }

    #[test]
    fn parses_only_the_active_branch_into_turns() {
        let session = parse(&fixture()).unwrap();
        assert_eq!(session.id, "01a0aaaa-0000-7000-8000-000000000001");
        assert_eq!(session.cwd, PathBuf::from("/tmp/zeron-tools-fixture"));
        assert_eq!(session.model.as_deref(), Some("anthropic/claude-opus-5-5"));
        assert_eq!(session.name.as_deref(), Some("Fixture Session"));
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
        // The abandoned branch's prompt must not leak into the transcript.
        assert!(
            session
                .messages
                .iter()
                .all(|m| !m.text().contains("abandoned"))
        );
        assert_eq!(session.messages[3].text(), "Second answer.");
    }

    #[test]
    fn tool_calls_fold_into_resolved_chips_with_diff_stats() {
        let session = parse(&fixture()).unwrap();
        let turn = &session.messages[1];
        assert!(
            matches!(&turn.parts[0], MessagePart::Reasoning { text, .. } if text == "Let me edit.")
        );
        let MessagePart::Tool {
            call,
            resolved,
            diff_stats,
            ..
        } = &turn.parts[1]
        else {
            panic!("expected a tool part, got {:?}", turn.parts[1]);
        };
        assert!(matches!(call, ToolCall::EditFile { path, .. } if path == "a.txt"));
        assert!(*resolved);
        assert_eq!(diff_stats.as_ref().map(Vec::len), Some(1));
        assert_eq!(turn.text(), "Done.");
        // From the first assistant message (10:00:02) to the last turn event (10:00:04).
        assert_eq!(turn.duration_ms, Some(2000));
    }
}
