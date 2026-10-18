//! Conversions from Zeron chats and harness-neutral sessions to MonoCode's
//! chat fields (`src/features/sessions/model/session.ts`).

use serde_json::{Value, json};
use zeron_doc::{MessagePart, MessageRole};
use zeron_proto::{HarnessId, ReasoningLevel, SandboxLevel, ToolCall};

use crate::sources::{Message, Seen};

/// MonoCode's harness id for a Zeron harness (what it stores in `harness`).
pub fn harness(h: HarnessId) -> Option<&'static str> {
    match h {
        HarnessId::Pi => Some("pi"),
        HarnessId::Codex => Some("codex"),
        _ => None,
    }
}

/// MonoCode's chat title: `<harness> · <title>` (`formatSessionTitle`), or
/// just the harness while untitled.
pub fn title(harness: &str, title: Option<&str>) -> String {
    match title.map(str::trim).filter(|t| !t.is_empty()) {
        Some(t) => format!("{harness} · {t}"),
        None => harness.to_owned(),
    }
}

/// `<harness>:<model>`: MonoCode's model id for a Zeron model id.
pub fn model(harness: &str, model: &str) -> String {
    format!("{harness}:{model}")
}

/// The settings key each harness reads its reasoning level from.
fn reasoning_key(harness: &str) -> &'static str {
    match harness {
        "codex" => "reasoningEffort",
        _ => "thinking",
    }
}

pub fn settings(harness: &str, reasoning: Option<ReasoningLevel>) -> Value {
    let mut out = serde_json::Map::new();
    if let Some(level) = reasoning.and_then(|r| serde_json::to_value(r).ok()) {
        out.insert(reasoning_key(harness).into(), level);
    }
    Value::Object(out)
}

/// MonoCode's permission mode for a Zeron sandbox, as MonoCode maps modes to
/// Codex sandboxes (`codexProtocol.ts` `baseCodexConfig`). Pi ignores both.
pub fn runtime_mode(sandbox: SandboxLevel) -> &'static str {
    match sandbox {
        SandboxLevel::ReadOnly => "supervised",
        SandboxLevel::WorkspaceWrite => "auto-accept-edits",
        SandboxLevel::DangerFullAccess => "full-access",
    }
}

/// Transcript blocks for `messages`: a user block per prompt (with the turn's
/// start and length, which [`seen`] reads back), then the turn's reasoning,
/// tool calls, and answers in order.
pub fn from_messages(messages: &[Message]) -> Vec<Value> {
    let mut out: Vec<Value> = vec![];
    let mut turn: Option<usize> = None;
    for m in messages {
        match m.role {
            MessageRole::User => {
                let mut text = m.text();
                if text.trim().is_empty() {
                    text = m
                        .parts
                        .iter()
                        .filter_map(|p| match p {
                            MessagePart::Image { name, .. } => Some(format!("[Image: {name}]")),
                            _ => None,
                        })
                        .collect::<Vec<_>>()
                        .join(" ");
                }
                out.push(json!({
                    "id": uuid::Uuid::new_v4().to_string(),
                    "role": "user",
                    "text": text,
                    "startedAt": m.created_ms,
                }));
                turn = Some(out.len() - 1);
            }
            MessageRole::Assistant => {
                for part in &m.parts {
                    if let Some(block) = block(part) {
                        out.push(block);
                    }
                }
                if let Some(i) = turn
                    && let Some(start) = out[i]["startedAt"].as_i64()
                {
                    let end = m.created_ms + m.duration_ms.unwrap_or(0);
                    let prev = out[i]["durationMs"].as_i64().unwrap_or(0);
                    out[i]["durationMs"] = json!(prev.max(end - start).max(0));
                }
            }
            MessageRole::System => {}
        }
    }
    out
}

fn block(part: &MessagePart) -> Option<Value> {
    let id = uuid::Uuid::new_v4().to_string();
    Some(match part {
        MessagePart::Text { text, .. } if !text.trim().is_empty() => {
            json!({"id": id, "role": "assistant", "text": text})
        }
        MessagePart::Reasoning { text, .. } if !text.trim().is_empty() => {
            json!({"id": id, "role": "reasoning", "text": text})
        }
        MessagePart::Tool { call, is_error, .. } => {
            let (kind, title) = tool(call);
            json!({
                "id": id,
                "role": "tool",
                "text": title,
                "tool": {
                    "title": title,
                    "kind": kind,
                    "status": if *is_error { "failed" } else { "completed" },
                },
            })
        }
        MessagePart::Error { message, .. } => {
            json!({"id": id, "role": "system", "text": message, "notice": "error"})
        }
        _ => return None,
    })
}

/// A tool call's MonoCode kind and one-line title.
fn tool(call: &ToolCall) -> (&'static str, String) {
    match call {
        ToolCall::Exec { command } => ("execute", command.clone()),
        ToolCall::ReadFile { path } => ("read", format!("Read {path}")),
        ToolCall::WriteFile { path, .. } => ("edit", format!("Write {path}")),
        ToolCall::EditFile { path, .. } => ("edit", format!("Edit {path}")),
        ToolCall::ApplyPatch { path } => (
            "edit",
            path.as_ref()
                .map_or("Apply patch".into(), |p| format!("Apply patch to {p}")),
        ),
        ToolCall::Search { pattern, path } => (
            "search",
            match path {
                Some(p) => format!("Search {pattern} in {p}"),
                None => format!("Search {pattern}"),
            },
        ),
        ToolCall::Glob { pattern } => ("search", format!("Find {pattern}")),
        ToolCall::WebFetch { url, .. } => ("fetch", format!("Fetch {url}")),
        ToolCall::WebSearch { query } => ("fetch", format!("Search the web for {query}")),
        ToolCall::Todo { .. } => ("other", "Update tasks".into()),
        ToolCall::Mcp { server, tool, .. } => ("other", format!("{server}: {tool}")),
        ToolCall::Unknown { name, .. } => ("other", name.clone()),
    }
}

/// How much of its session a transcript shows: the prompts sent (drafts
/// weren't), and when the newest turn ended (its start plus its length).
pub fn seen(blocks: &[Value]) -> Seen {
    let prompts: Vec<&Value> = blocks
        .iter()
        .filter(|b| b["role"] == "user" && b["draft"] != true)
        .collect();
    Seen {
        prompts: prompts.len(),
        end: prompts
            .iter()
            .filter_map(|b| {
                let start = b["startedAt"].as_i64().or_else(|| b["sentAt"].as_i64())?;
                Some(start + b["durationMs"].as_i64().unwrap_or(0))
            })
            .max(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn message(role: MessageRole, parts: Vec<MessagePart>, at: i64, took: Option<i64>) -> Message {
        Message {
            role,
            parts,
            created_ms: at,
            duration_ms: took,
            aborted: false,
        }
    }

    fn text(t: &str) -> MessagePart {
        MessagePart::Text {
            id: "p".into(),
            text: t.into(),
        }
    }

    #[test]
    fn turns_become_blocks_whose_end_matches_the_session() {
        let tool = MessagePart::Tool {
            id: "t".into(),
            call: ToolCall::Exec {
                command: "make test".into(),
            },
            is_error: true,
            resolved: true,
            output: None,
            diff: None,
            output_ref: None,
            output_bytes: None,
            diff_ref: None,
            diff_stats: None,
            subagent_ref: None,
            subagent_status: None,
            subagent_tail: None,
        };
        let thinking = MessagePart::Reasoning {
            id: "r".into(),
            text: "Run them".into(),
        };
        let blocks = from_messages(&[
            message(MessageRole::User, vec![text("Test it")], 1_000, None),
            message(
                MessageRole::Assistant,
                vec![thinking, tool, text("It fails.")],
                1_500,
                Some(2_000),
            ),
        ]);
        let roles: Vec<&str> = blocks.iter().map(|b| b["role"].as_str().unwrap()).collect();
        assert_eq!(roles, ["user", "reasoning", "tool", "assistant"]);
        assert_eq!(blocks[0]["durationMs"], 2_500);
        assert_eq!(blocks[2]["tool"]["kind"], "execute");
        assert_eq!(blocks[2]["tool"]["status"], "failed");
        assert_eq!(blocks[2]["text"], "make test");
        assert_eq!(
            seen(&blocks),
            Seen {
                prompts: 1,
                end: Some(3_500)
            }
        );
        assert_eq!(
            seen(&[json!({"role": "user", "draft": true})]),
            Seen::default()
        );
    }

    #[test]
    fn fields_map_to_monocode() {
        assert_eq!(title("pi", Some("Fix Login")), "pi · Fix Login");
        assert_eq!(title("codex", Some("  ")), "codex");
        assert_eq!(
            model("pi", "anthropic/claude-opus-5-5"),
            "pi:anthropic/claude-opus-5-5"
        );
        assert_eq!(
            settings("codex", Some(ReasoningLevel::XHigh)),
            json!({"reasoningEffort": "xhigh"})
        );
        assert_eq!(
            settings("pi", Some(ReasoningLevel::Medium)),
            json!({"thinking": "medium"})
        );
        assert_eq!(settings("pi", None), json!({}));
        assert_eq!(
            runtime_mode(SandboxLevel::WorkspaceWrite),
            "auto-accept-edits"
        );
        assert_eq!(runtime_mode(SandboxLevel::DangerFullAccess), "full-access");
    }
}
