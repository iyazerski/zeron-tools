//! Export chats as plain text for people and agents to read.
//!
//! It reads the running Zeron over its IPC. Messages render as Zeron's
//! `read_chat` shows them ([`transcript::render`]): continuation entries
//! fold into their turn, prose parts join, and every tool call becomes one
//! capped line.
//!
//! Two things are made explicit for readers: a forked chat starts with a copy
//! of its source's history (left out when the source is in the same export),
//! and a "user" message may come from another chat through Zeron's
//! `send_message` tool rather than from the person.

use std::collections::HashSet;
use std::fmt::Write;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use chrono::{DateTime, Duration, Local, NaiveDate, TimeZone, Utc};
use clap::ValueEnum;
use serde::Serialize;

use crate::zeron::transcript::{self, Entry, Part, RenderOptions, Rendered, Role};
use crate::zeron::types::{Chat, Space};
use crate::zeron::{Zeron, find_chat};

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

/// Which chats to export, and how.
pub struct Opts {
    /// Chat ids or id prefixes.
    pub chats: Vec<String>,
    /// A project name or folder.
    pub project: Option<String>,
    pub since: Option<DateTime<Utc>>,
    pub until: Option<DateTime<Utc>>,
    /// At most this many chats, the most recently active ones.
    pub limit: Option<usize>,
    pub skip_archived: bool,
    pub format: Format,
    pub render: RenderOptions,
    /// One file per chat in this folder, else everything to stdout.
    pub output: Option<PathBuf>,
}

/// Export the chats `opts` picks, oldest first, and report the count.
pub async fn run(zeron: &Zeron, opts: Opts) -> Result<()> {
    let (every, spaces) = tokio::try_join!(zeron.chats(), zeron.spaces())?;
    let space = opts
        .project
        .as_deref()
        .map(|p| find_space(&spaces, p))
        .transpose()?;
    let picked = opts
        .chats
        .iter()
        .map(|key| find_chat(&every, key).map(|c| c.id.as_str()))
        .collect::<Result<HashSet<_>>>()?;
    let mut selected: Vec<&Chat> = every
        .iter()
        .filter(|c| picked.is_empty() || picked.contains(c.id.as_str()))
        .filter(|c| space.is_none_or(|s| c.space_id.as_deref() == Some(s.id.as_str())))
        .filter(|c| active_between(c, opts.since, opts.until))
        .filter(|c| !(opts.skip_archived && c.archived))
        .collect();
    selected.sort_by_key(|c| std::cmp::Reverse(c.last_activity()));
    selected.truncate(opts.limit.unwrap_or(usize::MAX));
    selected.sort_by_key(|c| c.created_at);
    if let Some(dir) = &opts.output {
        std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    }
    // A fork's copied history is left out when its source is exported too.
    let ids: HashSet<&str> = selected.iter().map(|c| c.id.as_str()).collect();
    let (mut written, mut messages) = (0, 0);
    let mut stdout = std::io::stdout().lock();
    for chat in selected {
        let space = spaces
            .iter()
            .find(|s| Some(&s.id) == chat.space_id.as_ref());
        let entries = zeron.transcript(chat).await?;
        let exported = ExportedChat::new(chat, space, &entries, opts.render, &ids);
        if exported.messages.is_empty() {
            continue;
        }
        let text = exported.render(opts.format)?;
        match &opts.output {
            Some(dir) => std::fs::write(dir.join(exported.file_name(opts.format)), text)?,
            None => {
                use std::io::Write;
                // Separate markdown documents with a rule.
                if written > 0 && opts.format == Format::Markdown {
                    stdout.write_all(b"\n---\n\n")?;
                }
                stdout.write_all(text.as_bytes())?;
            }
        }
        written += 1;
        messages += exported.messages.len();
    }
    eprintln!(
        "exported {written} chat(s), {messages} message(s){}",
        opts.output
            .map(|d| format!(" to {}", d.display()))
            .unwrap_or_default()
    );
    Ok(())
}

/// The project named `arg` (case-insensitive) or at folder `arg`.
fn find_space<'a>(spaces: &'a [Space], arg: &str) -> Result<&'a Space> {
    let folder = crate::expand_tilde(arg);
    let folder = folder.canonicalize().unwrap_or(folder);
    spaces
        .iter()
        .find(|s| s.display_name().eq_ignore_ascii_case(arg))
        .or_else(|| spaces.iter().find(|s| Path::new(&s.path) == folder))
        .with_context(|| {
            let mut names: Vec<_> = spaces.iter().map(|s| s.display_name()).collect();
            names.sort_unstable();
            format!("no project {arg:?}; projects: {}", names.join(", "))
        })
}

/// A chat ready to write out.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ExportedChat {
    pub id: String,
    pub title: Option<String>,
    pub project: Option<String>,
    pub cwd: Option<String>,
    pub harness: Option<String>,
    pub model: Option<String>,
    pub archived: bool,
    pub created_at: DateTime<Utc>,
    pub last_activity: DateTime<Utc>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub forked_from: Option<Fork>,
    pub messages: Vec<ExportedMessage>,
}

/// Another chat, by id and title.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ChatRef {
    pub id: String,
    pub title: String,
}

/// Where a forked chat came from, and what happened to the copied history.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Fork {
    pub chat: ChatRef,
    /// Messages copied from the source when the fork was made.
    pub copied_messages: usize,
    /// Whether they are in `messages` (marked `copied`). They are left out
    /// when the source chat is in the same export, so nothing repeats.
    pub copies_included: bool,
}

/// One message, plus who really sent it.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ExportedMessage {
    #[serde(flatten)]
    pub message: Rendered,
    /// A user-role message another agent sent from this chat (Zeron's
    /// `send_message` tool), not something the person typed. `text` has
    /// the routing header removed.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub from_chat: Option<ChatRef>,
    /// Copied from the source chat of a fork.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub copied: bool,
}

impl ExportedChat {
    /// Render `entries` of `chat` (in `space`, if any). A fork's copied
    /// history is left out when its source is in `exported` (the ids of all
    /// chats in this export).
    pub fn new(
        chat: &Chat,
        space: Option<&Space>,
        entries: &[Entry],
        options: RenderOptions,
        exported: &HashSet<&str>,
    ) -> Self {
        let (forked_from, messages) = match fork_seam(entries) {
            Some((seam, source)) => {
                let copied = transcript::render(&entries[..seam], options);
                let include = !exported.contains(source.id.as_str());
                let fork = Fork {
                    chat: source,
                    copied_messages: copied.len(),
                    copies_included: include,
                };
                let copied = copied
                    .into_iter()
                    .filter(|_| include)
                    .map(|m| ExportedMessage::new(m, true));
                let own = transcript::render(&entries[seam + 1..], options)
                    .into_iter()
                    .map(|m| ExportedMessage::new(m, false));
                (Some(fork), copied.chain(own).collect())
            }
            None => (
                None,
                transcript::render(entries, options)
                    .into_iter()
                    .map(|m| ExportedMessage::new(m, false))
                    .collect(),
            ),
        };
        Self {
            forked_from,
            messages,
            id: chat.id.clone(),
            title: chat.title.clone(),
            project: space.map(|s| s.display_name().to_owned()),
            cwd: chat
                .cwd
                .clone()
                .or_else(|| chat.harness_session_cwd.clone())
                .or_else(|| space.map(|s| s.path.clone())),
            harness: chat.harness().map(str::to_owned),
            model: chat.model().map(str::to_owned),
            archived: chat.archived,
            created_at: chat.created_at,
            last_activity: chat.last_activity(),
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
        match (&self.harness, &self.model) {
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
        if let Some(fork) = &self.forked_from {
            let source = format!("{} ({})", fork.chat.title, fork.chat.id);
            meta.push(if fork.copies_included {
                format!(
                    "- Forked from: {source}. The first {} messages are copied from it.",
                    fork.copied_messages
                )
            } else {
                format!(
                    "- Forked from: {source}. The {} messages copied from it are left out; they are in that chat's export.",
                    fork.copied_messages
                )
            });
        }
        out.push_str(&meta.join("\n"));
        out.push('\n');
        let mut copied_before = false;
        for em in &self.messages {
            let m = &em.message;
            // One blank line between blocks, however the previous one ended.
            out.truncate(out.trim_end().len());
            out.push_str("\n\n");
            if copied_before && !em.copied {
                out.push_str("---\n\n*Forked here. Messages below are this chat's own.*\n\n");
            }
            copied_before = em.copied;
            let role = match (&em.from_chat, m.role) {
                (Some(from), _) => format!(
                    "Message from chat {} ({})",
                    from.title,
                    crate::short_id(&from.id)
                ),
                (None, Role::User) => "User".into(),
                (None, Role::Assistant) => "Assistant".into(),
                (None, Role::System | Role::Other) => "System".into(),
            };
            let at = DateTime::from_timestamp_millis(m.created_at)
                .map(local)
                .unwrap_or_default();
            let copied = if em.copied { ", copied" } else { "" };
            let _ = write!(out, "## {role} ({at}{copied})\n\n");
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

impl ExportedMessage {
    fn new(mut message: Rendered, copied: bool) -> Self {
        let mut from_chat = None;
        if message.role == Role::User
            && let Some((from, body)) = split_agent_message(&message.text)
        {
            message.text = body.to_owned();
            from_chat = Some(from);
        }
        Self {
            message,
            from_chat,
            copied,
        }
    }
}

/// The fork seam (Zeron's fork RPC writes one entry with a `Fork` part:
/// everything above it was copied from the source chat), as its index and
/// the source.
fn fork_seam(entries: &[Entry]) -> Option<(usize, ChatRef)> {
    entries.iter().enumerate().find_map(|(i, e)| {
        e.parts.iter().find_map(|p| match p {
            Part::Fork {
                source_chat_id,
                source_title,
            } => Some((
                i,
                ChatRef {
                    id: source_chat_id.clone(),
                    title: source_title.clone(),
                },
            )),
            _ => None,
        })
    })
}

/// Split a message another chat sent through Zeron's `send_message` into the
/// sender and the body. Zeron stores no sender field: `crates/mcp/src/tools.rs`
/// `attribute` prefixes the text, and the desktop transcript parses it back
/// (`crates/ui/src/transcript.rs` `agent_message_display`), as here.
fn split_agent_message(text: &str) -> Option<(ChatRef, &str)> {
    let rest = text.strip_prefix("[Message from Zeron chat ")?;
    let (header, body) = rest.split_once("]\n\n")?;
    let (label, id) =
        header.rsplit_once(". Reply to it with the Zeron `send_message` tool, chat ")?;
    let id = id.strip_suffix('.')?;
    // The label is "<title> (<short id>)", or just the short id when untitled.
    let title = match label.rsplit_once(" (") {
        Some((title, short)) if short.strip_suffix(')').is_some_and(|s| id.starts_with(s)) => title,
        _ if id.starts_with(label) => "",
        _ => label,
    };
    Some((
        ChatRef {
            id: id.to_owned(),
            title: title.to_owned(),
        },
        body,
    ))
}

/// Whether a chat was active within `[since, until)`.
pub fn active_between(
    chat: &Chat,
    since: Option<DateTime<Utc>>,
    until: Option<DateTime<Utc>>,
) -> bool {
    since.is_none_or(|t| chat.last_activity() >= t) && until.is_none_or(|t| chat.created_at < t)
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
    crate::short_path(Path::new(path))
}

#[cfg(test)]
mod tests {
    use serde_json::{Value, json};

    use super::*;
    use crate::zeron::transcript::tests::entry;

    fn chat() -> Chat {
        serde_json::from_value(json!({
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

    fn text(t: &str) -> Value {
        json!({ "kind": "text", "id": "p", "text": t })
    }

    fn exported(options: RenderOptions) -> ExportedChat {
        let tool = json!({ "kind": "tool", "id": "t", "call": { "kind": "exec", "command": "make test" }, "resolved": true });
        let thinking = json!({ "kind": "reasoning", "id": "r", "text": "Check the tests" });
        ExportedChat::new(
            &chat(),
            None,
            &[
                entry("u1", "user", None, json!([text("Run the tests")])),
                entry("a1", "assistant", None, json!([thinking, tool])),
                entry("a2", "assistant", Some("a1"), json!([text("All green.")])),
            ],
            options,
            &HashSet::new(),
        )
    }

    /// A fork of chat `src`: two copied messages, the seam, then its own turn,
    /// which another chat sent.
    fn fork(exported: &[&str]) -> ExportedChat {
        let seam = json!({ "kind": "fork", "id": "f", "sourceChatId": "src-chat-id", "sourceTitle": "Review Rollout" });
        let peer = "[Message from Zeron chat Fault Injector (127d9f07). Reply to it with the Zeron `send_message` tool, chat 127d9f07-dbd0-4c3e.]\n\nOver to you.";
        ExportedChat::new(
            &chat(),
            None,
            &[
                entry("u1", "user", None, json!([text("Review the rollout")])),
                entry("a1", "assistant", None, json!([text("Looks fine.")])),
                entry("fork:x", "system", None, json!([seam])),
                entry("u2", "user", None, json!([text(peer)])),
                entry("a2", "assistant", None, json!([text("Thanks.")])),
            ],
            RenderOptions::default(),
            &exported.iter().copied().collect(),
        )
    }

    #[test]
    fn fork_drops_copies_only_when_its_source_is_exported() {
        let alone = fork(&[]);
        let fork_of = alone.forked_from.clone().unwrap();
        assert_eq!(
            (fork_of.chat.id.as_str(), fork_of.copied_messages),
            ("src-chat-id", 2)
        );
        assert!(fork_of.copies_included);
        let copied: Vec<bool> = alone.messages.iter().map(|m| m.copied).collect();
        assert_eq!(copied, [true, true, false, false]);
        let md = alone.render(Format::Markdown).unwrap();
        assert!(
            md.contains("- Forked from: Review Rollout (src-chat-id). The first 2 messages"),
            "{md}"
        );
        assert!(
            md.contains("Looks fine.\n\n---\n\n*Forked here. Messages below are this chat's own.*\n\n## Message from"),
            "{md}"
        );

        let with_source = fork(&["src-chat-id"]);
        assert!(!with_source.forked_from.unwrap().copies_included);
        assert_eq!(with_source.messages.len(), 2);
        assert!(with_source.messages.iter().all(|m| !m.copied));
    }

    #[test]
    fn messages_from_other_chats_are_attributed() {
        let e = fork(&["src-chat-id"]);
        let from = e.messages[0].from_chat.clone().unwrap();
        assert_eq!(
            (from.id.as_str(), from.title.as_str()),
            ("127d9f07-dbd0-4c3e", "Fault Injector")
        );
        assert_eq!(e.messages[0].message.text, "Over to you.");
        let md = e.render(Format::Markdown).unwrap();
        assert!(
            md.contains("## Message from chat Fault Injector (127d9f07-dbd0)"),
            "{md}"
        );
        assert!(!md.contains("## User"), "{md}");
        let v: serde_json::Value = serde_json::from_str(&e.render(Format::Jsonl).unwrap()).unwrap();
        assert_eq!(v["messages"][0]["role"], "user");
        assert_eq!(v["messages"][0]["fromChat"]["title"], "Fault Injector");
        assert!(v["messages"][1].get("fromChat").is_none());
        assert!(v["messages"][1].get("copied").is_none());
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
