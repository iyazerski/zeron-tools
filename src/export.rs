//! Export chats as plain text for people and agents to read.
//!
//! Messages go through Zeron's own transcript renderer
//! (`zeron_mcp::render_entries`, what its `read_chat` MCP tool returns):
//! continuation entries fold into their turn, prose parts join, and every
//! tool call becomes one capped line.

use std::fmt::Write;

use anyhow::{Context, Result, bail};
use chrono::{DateTime, Duration, Local, NaiveDate, TimeZone, Utc};
use clap::ValueEnum;
use serde::Serialize;
use zeron_doc::{MessageRole, SessionMessageEntry};
use zeron_mcp::{RenderOptions, RenderedMessage, render_entries};
use zeron_proto::{Chat, HarnessId, Space};

#[derive(Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum Format {
    /// One document per chat: metadata list, then a heading per message
    Markdown,
    /// One JSON object per chat per line (metadata plus rendered messages)
    Jsonl,
}

impl Format {
    pub fn extension(self) -> &'static str {
        match self {
            Self::Markdown => "md",
            Self::Jsonl => "jsonl",
        }
    }
}

/// A chat ready to write out.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ExportedChat {
    pub id: String,
    pub title: Option<String>,
    pub project: Option<String>,
    pub cwd: Option<String>,
    pub harness: Option<HarnessId>,
    pub model: Option<String>,
    pub archived: bool,
    pub created_at: DateTime<Utc>,
    pub last_activity: DateTime<Utc>,
    pub messages: Vec<RenderedMessage>,
}

impl ExportedChat {
    /// Render `entries` of `chat` (in `space`, if any) with Zeron's renderer.
    pub fn new(
        chat: &Chat,
        space: Option<&Space>,
        entries: &[SessionMessageEntry],
        options: RenderOptions,
    ) -> Self {
        Self {
            id: chat.id.clone(),
            title: chat.title.clone(),
            project: space.map(|s| s.display_name().to_owned()),
            cwd: chat
                .cwd
                .clone()
                .or_else(|| chat.harness_session_cwd.clone())
                .or_else(|| space.map(|s| s.path.clone())),
            harness: chat.config.as_ref().map(|c| c.harness),
            model: chat.config.as_ref().and_then(|c| c.model.clone()),
            archived: chat.archived,
            created_at: chat.created_at,
            last_activity: last_activity(chat),
            messages: render_entries(entries, options),
        }
    }

    pub fn render(&self, format: Format) -> Result<String> {
        Ok(match format {
            Format::Markdown => self.markdown(),
            Format::Jsonl => serde_json::to_string(self)? + "\n",
        })
    }

    /// `<created date>-<title slug>-<short id>.<ext>`: sorts by date, stays unique.
    pub fn file_name(&self, format: Format) -> String {
        let slug: String = self
            .title
            .as_deref()
            .unwrap_or("untitled")
            .chars()
            .map(|c| {
                if c.is_ascii_alphanumeric() {
                    c.to_ascii_lowercase()
                } else {
                    '-'
                }
            })
            .collect();
        let slug = slug
            .split('-')
            .filter(|w| !w.is_empty())
            .collect::<Vec<_>>()
            .join("-");
        format!(
            "{}-{}-{}.{}",
            self.created_at.with_timezone(&Local).format("%Y-%m-%d"),
            slug.get(..60).unwrap_or(&slug).trim_end_matches('-'),
            crate::short_id(&self.id),
            format.extension()
        )
    }

    fn markdown(&self) -> String {
        let mut out = format!("# {}\n\n", self.title.as_deref().unwrap_or("Untitled"));
        let mut meta = vec![format!("- Chat: {}", self.id)];
        match (&self.project, &self.cwd) {
            (Some(p), Some(cwd)) => meta.push(format!("- Project: {p} ({})", tilde(cwd))),
            (Some(p), None) => meta.push(format!("- Project: {p}")),
            (None, Some(cwd)) => meta.push(format!("- Folder: {}", tilde(cwd))),
            (None, None) => {}
        }
        let harness = self
            .harness
            .and_then(|h| serde_json::to_value(h).ok())
            .and_then(|v| v.as_str().map(str::to_owned));
        match (harness, &self.model) {
            (Some(h), Some(m)) => meta.push(format!("- Agent: {h}, {m}")),
            (Some(h), None) => meta.push(format!("- Agent: {h}")),
            _ => {}
        }
        meta.push(format!(
            "- Time: {} to {}",
            local(self.created_at),
            local(self.last_activity)
        ));
        if self.archived {
            meta.push("- Archived".into());
        }
        out.push_str(&meta.join("\n"));
        out.push('\n');
        for m in &self.messages {
            let role = match m.role {
                MessageRole::User => "User",
                MessageRole::Assistant => "Assistant",
                MessageRole::System => "System",
            };
            let at = DateTime::from_timestamp_millis(m.created_at)
                .map(local)
                .unwrap_or_default();
            let _ = write!(out, "\n## {role} ({at})\n\n");
            if let Some(reasoning) = &m.reasoning {
                out.push_str("Reasoning:\n\n");
                for line in reasoning.lines() {
                    out.push_str(if line.is_empty() { ">" } else { "> " });
                    out.push_str(line);
                    out.push('\n');
                }
                out.push('\n');
            }
            if !m.text.is_empty() {
                out.push_str(demote_headings(&m.text).trim_end());
                out.push_str("\n\n");
            }
            for (label, lines) in [("Tools", &m.tools), ("Errors", &m.errors)] {
                if !lines.is_empty() {
                    let _ = writeln!(out, "{label}:");
                    for line in lines {
                        let _ = writeln!(out, "- {}", line.replace('\n', " "));
                    }
                    out.push('\n');
                }
            }
        }
        out.trim_end().to_owned() + "\n"
    }
}

/// A chat's last activity: its newest message, else its creation.
pub fn last_activity(chat: &Chat) -> DateTime<Utc> {
    chat.last_message_at.unwrap_or(chat.created_at)
}

/// Whether a chat was active within `[since, until)`.
pub fn active_between(
    chat: &Chat,
    since: Option<DateTime<Utc>>,
    until: Option<DateTime<Utc>>,
) -> bool {
    since.is_none_or(|t| last_activity(chat) >= t) && until.is_none_or(|t| chat.created_at < t)
}

/// Parse `--since`: a local date (`2026-09-17`) or an age (`30d`, `4w`).
pub fn parse_since(arg: &str) -> Result<DateTime<Utc>> {
    let age = |unit: char, days: i64| -> Option<Result<DateTime<Utc>>> {
        let n = arg.strip_suffix(unit)?;
        Some(
            n.parse::<i64>()
                .map(|n| Utc::now() - Duration::days(n * days))
                .with_context(|| format!("bad age {arg:?}")),
        )
    };
    age('d', 1)
        .or_else(|| age('w', 7))
        .unwrap_or_else(|| local_midnight(arg, 0))
}

/// Parse `--until`: a local date, included (the window ends at the next midnight).
pub fn parse_until(arg: &str) -> Result<DateTime<Utc>> {
    local_midnight(arg, 1)
}

fn local_midnight(arg: &str, plus_days: i64) -> Result<DateTime<Utc>> {
    let date = NaiveDate::parse_from_str(arg, "%Y-%m-%d")
        .with_context(|| format!("expected YYYY-MM-DD, or an age like 30d or 4w: {arg:?}"))?
        + Duration::days(plus_days);
    let Some(t) = Local
        .from_local_datetime(&date.and_hms_opt(0, 0, 0).unwrap_or_default())
        .earliest()
    else {
        bail!("{arg} has no local midnight");
    };
    Ok(t.with_timezone(&Utc))
}

/// Push markdown headings in a message body two levels down (to at most
/// `######`), outside code fences, so `#` (chat) and `##` (message) stay
/// the document's own structure.
fn demote_headings(text: &str) -> String {
    let mut fence: Option<&str> = None;
    let mut out = String::with_capacity(text.len() + 16);
    for line in text.split_inclusive('\n') {
        let trimmed = line.trim_start();
        let marker = ["```", "~~~"].into_iter().find(|m| trimmed.starts_with(m));
        match (fence, marker) {
            (None, Some(m)) => fence = Some(m),
            (Some(open), Some(m)) if open == m => fence = None,
            _ => {}
        }
        let level = line.bytes().take_while(|b| *b == b'#').count();
        if fence.is_none()
            && marker.is_none()
            && (1..=6).contains(&level)
            && line[level..].starts_with([' ', '\t'])
        {
            out.push_str(&"#".repeat((level + 2).min(6)));
            out.push_str(&line[level..]);
        } else {
            out.push_str(line);
        }
    }
    out
}

fn local(t: DateTime<Utc>) -> String {
    t.with_timezone(&Local).format("%Y-%m-%d %H:%M").to_string()
}

fn tilde(path: &str) -> String {
    crate::short_path(std::path::Path::new(path))
}

#[cfg(test)]
mod tests {
    use zeron_doc::MessagePart;
    use zeron_proto::ToolCall;

    use super::*;

    fn chat() -> Chat {
        serde_json::from_value(serde_json::json!({
            "id": "01a0d3bf-536f-79b2-ae0b-bcf3502434d5",
            "deviceId": "d",
            "title": "Advance PO_GUID IDR demo",
            "archived": false,
            "createdAt": "2026-09-24T14:08:46Z",
            "lastMessageAt": "2026-09-24T17:04:22Z",
            "harnessSessionCwd": "/work/guid-service",
            "config": { "harness": "codex", "model": "gpt-6-sol", "sandbox": "workspace-write" },
        }))
        .unwrap()
    }

    fn entry(id: &str, role: MessageRole, parts: Vec<MessagePart>) -> SessionMessageEntry {
        SessionMessageEntry {
            id: id.into(),
            role,
            parts,
            created_at: 1_790_258_926_000,
            device_id: "d".into(),
            status: None,
            continuation_of: None,
            duration_ms: None,
        }
    }

    fn text(t: &str) -> MessagePart {
        MessagePart::Text {
            id: "p".into(),
            text: t.into(),
        }
    }

    fn exported(options: RenderOptions) -> ExportedChat {
        let tool = MessagePart::Tool {
            id: "t".into(),
            call: ToolCall::Exec {
                command: "make test".into(),
            },
            is_error: false,
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
            text: "Check the tests".into(),
        };
        let mut cont = entry("a2", MessageRole::Assistant, vec![text("All green.")]);
        cont.continuation_of = Some("a1".into());
        ExportedChat::new(
            &chat(),
            None,
            &[
                entry("u1", MessageRole::User, vec![text("Run the tests")]),
                entry("a1", MessageRole::Assistant, vec![thinking, tool]),
                cont,
            ],
            options,
        )
    }

    #[test]
    fn markdown_lists_metadata_messages_and_tools() {
        let md = exported(RenderOptions::default())
            .render(Format::Markdown)
            .unwrap();
        assert!(md.starts_with("# Advance PO_GUID IDR demo\n\n- Chat: 01a0d3bf-536f-79b2-ae0b-bcf3502434d5\n- Folder: /work/guid-service\n- Agent: codex, gpt-6-sol\n"), "{md}");
        // The continuation folds into its turn; reasoning is off by default.
        assert_eq!(md.matches("## Assistant").count(), 1, "{md}");
        assert!(
            md.contains("All green.\n\nTools:\n- exec: make test\n"),
            "{md}"
        );
        assert!(!md.contains("Check the tests"), "{md}");
        let with_reasoning = exported(RenderOptions {
            include_reasoning: true,
            include_tools: false,
        })
        .render(Format::Markdown)
        .unwrap();
        assert!(with_reasoning.contains("Reasoning:\n\n> Check the tests\n"));
        assert!(!with_reasoning.contains("Tools:"));
    }

    #[test]
    fn message_headings_nest_under_the_message() {
        let body = "## Done\n\n```sh\n# a comment\n```\n##### Deep\n#hashtag\n";
        assert_eq!(
            demote_headings(body),
            "#### Done\n\n```sh\n# a comment\n```\n###### Deep\n#hashtag\n"
        );
    }

    #[test]
    fn jsonl_is_one_line_per_chat() {
        let line = exported(RenderOptions::default())
            .render(Format::Jsonl)
            .unwrap();
        assert_eq!(line.lines().count(), 1);
        let v: serde_json::Value = serde_json::from_str(&line).unwrap();
        assert_eq!(v["messages"].as_array().unwrap().len(), 2);
        assert_eq!(v["harness"], "codex");
    }

    #[test]
    fn file_names_sort_by_date_and_stay_unique() {
        let e = exported(RenderOptions::default());
        assert!(
            e.file_name(Format::Markdown)
                .ends_with("-advance-po-guid-idr-demo-01a0d3bf-536f.md")
        );
    }

    #[test]
    fn date_window_matches_overlapping_chats() {
        let c = chat();
        let at = |s: &str| Some(s.parse::<DateTime<Utc>>().unwrap());
        assert!(active_between(&c, at("2026-09-24T16:00:00Z"), None));
        assert!(!active_between(&c, at("2026-09-24T18:00:00Z"), None));
        assert!(active_between(&c, None, at("2026-09-24T15:00:00Z")));
        assert!(!active_between(&c, None, at("2026-09-24T14:00:00Z")));
        let since = parse_since("2026-09-17").unwrap();
        assert_eq!(parse_until("2026-09-16").unwrap(), since);
        let week = Utc::now() - parse_since("1w").unwrap();
        assert!((week - Duration::days(7)).num_seconds().abs() < 5);
        assert!(parse_since("soon").is_err());
    }
}
