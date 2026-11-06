//! What the companion reads from the phone's tools, and the wording the phone
//! app (`relay/assets/app.js`) derives from it, so both read the same.

use chrono::{DateTime, Utc};
use serde::Deserialize;

/// A chat as `list_chats` and `chat_view` summarize it.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Chat {
    pub id: String,
    pub title: Option<String>,
    /// Zeron's status label, like `working` or `working (stale — …)`.
    #[serde(default)]
    pub status: String,
    pub harness: Option<String>,
    pub model: Option<String>,
    pub reasoning: Option<String>,
    pub project: Option<Project>,
    pub branch: Option<String>,
    pub last_message_at: Option<DateTime<Utc>>,
    pub created_at: DateTime<Utc>,
    #[serde(default)]
    pub last_message_preview: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct Project {
    pub id: String,
    pub name: String,
    #[serde(default)]
    pub path: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Status {
    Idle,
    Working { stale: bool },
    Input,
    Failed,
}

impl Chat {
    /// The status as the app groups it: by its first word.
    pub fn status(&self) -> Status {
        match self.status.split(' ').next().unwrap_or_default() {
            "working" => Status::Working {
                stale: self.status.contains("stale"),
            },
            "awaitingInput" => Status::Input,
            "errored" => Status::Failed,
            _ => Status::Idle,
        }
    }

    pub fn is_working(&self) -> bool {
        matches!(self.status(), Status::Working { .. })
    }

    pub fn title(&self) -> &str {
        self.title
            .as_deref()
            .filter(|t| !t.is_empty())
            .unwrap_or("Untitled chat")
    }

    /// When the chat last moved, for sorting and its age.
    pub fn active_at(&self) -> DateTime<Utc> {
        self.last_message_at.unwrap_or(self.created_at)
    }
}

#[derive(Debug, Deserialize)]
pub struct Chats {
    pub chats: Vec<Chat>,
}

/// `chat_view`'s answer; `read` is `None` when nothing changed since `known`.
#[derive(Debug, Deserialize)]
pub struct ChatView {
    pub chat: Chat,
    pub version: String,
    pub read: Option<Read>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Read {
    pub messages: Vec<Message>,
    #[serde(default)]
    pub older_remaining: usize,
    pub pending_input: Option<PendingInput>,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Message {
    pub id: String,
    pub role: Role,
    /// Epoch milliseconds.
    pub created_at: i64,
    pub status: Option<String>,
    pub parts: Vec<Part>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    User,
    Assistant,
    #[serde(other)]
    System,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(tag = "kind", rename_all = "lowercase")]
pub enum Part {
    Text { text: String },
    Tools { tools: Vec<String> },
    Error { error: String },
}

impl Message {
    /// The message's prose, joined as `read_chat` joins it.
    pub fn text(&self) -> String {
        let texts: Vec<&str> = self
            .parts
            .iter()
            .filter_map(|p| match p {
                Part::Text { text } => Some(text.as_str()),
                _ => None,
            })
            .collect();
        texts.join("\n\n")
    }
}

/// Questions the agent waits on, as Zeron asks them.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PendingInput {
    pub request_id: String,
    pub questions: Vec<Question>,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Question {
    pub id: String,
    #[serde(default)]
    pub header: String,
    #[serde(default)]
    pub question: String,
    #[serde(default)]
    pub options: Vec<String>,
    #[serde(default)]
    pub multi_select: bool,
    #[serde(default)]
    pub prefill: Option<String>,
}

/// `composer_info`: what the composer shows under the text.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ComposerInfo {
    pub branch: Option<String>,
    /// Set for a git project, the only place the branch shows.
    pub checkout: Option<serde_json::Value>,
    pub context_usage: Option<ContextUsage>,
    pub harness: Option<String>,
    pub model: Option<String>,
    pub reasoning: Option<String>,
}

#[derive(Debug, Clone, Copy, Deserialize)]
pub struct ContextUsage {
    pub tokens: Option<u64>,
    pub window: Option<u64>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Model {
    pub id: String,
    pub label: Option<String>,
    #[serde(default)]
    pub reasoning_levels: Vec<String>,
}

#[derive(Debug, Deserialize)]
pub struct Models {
    pub models: Vec<Model>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Harness {
    pub id: String,
    #[serde(default)]
    pub available: bool,
    #[serde(default)]
    pub reasoning_levels: Vec<String>,
}

#[derive(Debug, Deserialize)]
pub struct Harnesses {
    pub harnesses: Vec<Harness>,
}

#[derive(Debug, Deserialize)]
pub struct Projects {
    pub projects: Vec<Project>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Skill {
    pub name: String,
    #[serde(default)]
    pub description: String,
    /// The reference that invokes the skill when sent.
    #[serde(rename = "ref")]
    pub reference: String,
}

#[derive(Debug, Deserialize)]
pub struct Skills {
    pub skills: Vec<Skill>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Created {
    pub chat_id: String,
}

/// A model and reasoning pick, as `set_chat_model` saves it.
#[derive(Debug, Deserialize)]
pub struct Picked {
    pub model: Option<String>,
    pub reasoning: Option<String>,
}

/// Compact age, like the desktop sidebar: now, 34m, 4h, 2d, 3w.
pub fn ago(when: DateTime<Utc>, now: DateTime<Utc>) -> String {
    let s = (now - when).num_seconds().max(0);
    if s < 60 {
        "now".to_owned()
    } else if s < 3600 {
        format!("{}m", s / 60)
    } else if s < 86_400 {
        format!("{}h", s / 3600)
    } else if s < 604_800 {
        format!("{}d", s / 86_400)
    } else {
        format!("{}w", s / 604_800)
    }
}

pub fn harness_name(id: &str) -> &str {
    match id {
        "pi" => "Pi",
        "codex" => "Codex",
        "claude-code" => "Claude Code",
        "cursor" => "Cursor",
        "devin" => "Devin",
        "grok" => "Grok",
        "hermes" => "Hermes",
        "antigravity" => "Antigravity",
        "opencode" => "OpenCode",
        "mock" => "Mock",
        other => other,
    }
}

/// A model id without its provider: `anthropic/claude-x` reads `claude-x`.
pub fn short_model(model: &str) -> &str {
    model.rsplit('/').next().unwrap_or(model)
}

pub fn reasoning_name(level: &str) -> String {
    if level == "xhigh" {
        return "X-High".to_owned();
    }
    let mut chars = level.chars();
    chars
        .next()
        .map_or_else(String::new, |c| c.to_uppercase().chain(chars).collect())
}

/// The reasoning levels for `model`: its own, else its harness's.
pub fn levels_of<'a>(models: &'a [Model], harness: &'a [String], model: &str) -> &'a [String] {
    models
        .iter()
        .find(|m| m.id == model)
        .map(|m| m.reasoning_levels.as_slice())
        .filter(|l| !l.is_empty())
        .unwrap_or(harness)
}

/// The desktop's project tone, by FNV-1a of the project path.
pub fn tone_index(path: &str) -> usize {
    let hash = path.bytes().fold(2_166_136_261u32, |h, b| {
        (h ^ u32::from(b)).wrapping_mul(16_777_619)
    });
    (hash % 8) as usize
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolState {
    Done,
    Running,
    Failed,
}

/// One tool call line (`exec: cargo test → ok (running)`), split up.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolCall<'a> {
    /// `exec`, `read`, … or `tool` for one the app doesn't name.
    pub kind: &'a str,
    pub arg: &'a str,
    pub output: &'a str,
    pub state: ToolState,
}

const TOOL_KINDS: [&str; 11] = [
    "exec",
    "read",
    "write",
    "edit",
    "patch",
    "search",
    "glob",
    "web search",
    "fetch",
    "todo",
    "mcp",
];

pub fn parse_tool(line: &str) -> ToolCall<'_> {
    let (text, state) = if let Some(t) = line.strip_suffix(" (running)") {
        (t, ToolState::Running)
    } else if let Some(t) = line.strip_suffix(" (error)") {
        (t, ToolState::Failed)
    } else {
        (line, ToolState::Done)
    };
    let (head, output) = text.split_once(" → ").unwrap_or((text, ""));
    let named = TOOL_KINDS.iter().find_map(|kind| {
        let rest = head.strip_prefix(kind)?;
        if rest.is_empty() {
            return Some((*kind, ""));
        }
        rest.strip_prefix(':')
            .map(|arg| (*kind, arg.strip_prefix(' ').unwrap_or(arg)))
    });
    let (kind, arg) = named.unwrap_or(("tool", head));
    ToolCall {
        kind,
        arg,
        output,
        state,
    }
}

/// How the desktop labels a call: its verb ("Run cargo test").
pub fn tool_verb(kind: &str) -> &'static str {
    match kind {
        "exec" => "Run",
        "read" => "Read",
        "write" => "Write",
        "edit" | "patch" => "Edit",
        "search" | "glob" | "web search" => "Search",
        "fetch" => "Fetch",
        "todo" => "Todo",
        _ => "Tool",
    }
}

fn plural(n: usize, one: &str, many: &str) -> String {
    format!("{n} {}", if n == 1 { one } else { many })
}

/// The desktop's tool group summary (`zeron_proto::view::tool_group_summary`).
pub fn tool_summary(tools: &[ToolCall]) -> String {
    let mut edited: Vec<&str> = Vec::new();
    let (mut commands, mut reads, mut searches, mut fetches, mut todos, mut other) =
        (0, 0, 0, 0, 0, 0);
    for t in tools {
        match t.kind {
            "exec" => commands += 1,
            "write" | "edit" | "patch" => {
                let file = if t.arg.is_empty() { "patch" } else { t.arg };
                if !edited.contains(&file) {
                    edited.push(file);
                }
            }
            "read" => reads += 1,
            "search" | "glob" | "web search" => searches += 1,
            "fetch" => fetches += 1,
            "todo" => todos += 1,
            _ => other += 1,
        }
    }
    let failed = tools
        .iter()
        .filter(|t| t.state == ToolState::Failed)
        .count();
    let mut parts = Vec::new();
    let mut add = |n: usize, word: &str, one: &str, many: &str| {
        if n > 0 {
            parts.push(format!("{word} {}", plural(n, one, many)));
        }
    };
    add(commands, "ran", "command", "commands");
    add(edited.len(), "edited", "file", "files");
    add(reads, "read", "file", "files");
    add(searches, "searched", "time", "times");
    add(fetches, "fetched", "page", "pages");
    if todos > 0 {
        parts.push("updated todos".to_owned());
    }
    if other > 0 {
        parts.push(format!("called {}", plural(other, "tool", "tools")));
    }
    if failed > 0 {
        parts.push(format!("{failed} failed"));
    }
    let s = parts.join(" · ");
    let mut chars = s.chars();
    chars
        .next()
        .map_or_else(String::new, |c| c.to_uppercase().chain(chars).collect())
}

/// What Zeron's composer appends to a message with images: the agent opens
/// them by path, and the transcript finds them there.
const IMAGES_HEAD: &str = "\n\nAttached images (local files — open them to view):";

/// The message Zeron's composer sends for `text` with images saved at
/// `paths`: a `zeron-image` reference per image, then their paths.
pub fn with_images(text: &str, paths: &[String]) -> String {
    if paths.is_empty() {
        return text.to_owned();
    }
    let mut out = text.trim_end().to_owned();
    for n in 1..=paths.len() {
        let gap = if out.is_empty() { "" } else { " " };
        out.push_str(&format!("{gap}[Image {n}](zeron-image:{n})"));
    }
    out.push_str(IMAGES_HEAD);
    for path in paths {
        out.push_str(&format!("\n- {path}"));
    }
    out
}

/// Split a sent message into its text and its attached images' paths.
pub fn split_images(text: &str) -> (&str, Vec<&str>) {
    let Some(at) = text.rfind(IMAGES_HEAD) else {
        return (text, Vec::new());
    };
    let paths = text[at + IMAGES_HEAD.len()..]
        .lines()
        .filter_map(|l| l.strip_prefix("- "))
        .collect();
    (&text[..at], paths)
}

/// A message from another Zeron chat starts with a routing header; split off
/// the chat's name.
pub fn split_from(text: &str) -> (Option<&str>, &str) {
    const HEAD: &str = "[Message from Zeron chat ";
    const REPLY: &str = ". Reply to it with the Zeron `send_message` tool, chat ";
    let found = (|| {
        let rest = text.strip_prefix(HEAD)?;
        let cut = rest.find(REPLY)?;
        let after = &rest[cut + REPLY.len()..];
        let close = after.find("]\n\n")?;
        (!after[..close].contains(']')).then(|| (&rest[..cut], &after[close + 3..]))
    })();
    match found {
        Some((from, body)) if !from.is_empty() => (Some(from), body),
        _ => (None, text),
    }
}

/// `text` cut at Zeron's composer references (`[$commit](zeron-invoke:…)`):
/// `(piece, is_reference)`, with each reference as its label.
pub fn references(text: &str) -> Vec<(&str, bool)> {
    let mut out = Vec::new();
    let mut rest = text;
    let mut plain = 0;
    while let Some(open) = rest[plain..].find('[').map(|i| i + plain) {
        let parsed = (|| {
            let close = rest[open + 1..].find(']')? + open + 1;
            let label = &rest[open + 1..close];
            let target = rest[close + 1..].strip_prefix("(zeron-")?;
            let colon = target.find(':')?;
            if colon == 0
                || !target[..colon]
                    .bytes()
                    .all(|b| b.is_ascii_lowercase() || b == b'-')
            {
                return None;
            }
            let end = target[colon..].find(|c: char| c == ')' || c.is_whitespace())? + colon;
            target[end..]
                .starts_with(')')
                .then(|| (label, close + 1 + "(zeron-".len() + end + 1))
        })();
        match parsed {
            Some((label, end)) if !label.is_empty() => {
                if open > 0 {
                    out.push((&rest[..open], false));
                }
                out.push((label, true));
                rest = &rest[end..];
                plain = 0;
            }
            _ => plain = open + 1,
        }
    }
    if !rest.is_empty() {
        out.push((rest, false));
    }
    out
}

/// The `$` word the caret at byte `caret` ends: where its `$` is, and the
/// lowercased text after it.
pub fn skill_query(text: &str, caret: usize) -> Option<(usize, String)> {
    let before = text.get(..caret)?;
    let start = before.rfind('$')?;
    let word = &before[start + 1..];
    let lead = before[..start].chars().next_back();
    let ok = lead.is_none_or(char::is_whitespace)
        && word
            .chars()
            .all(|c| c.is_alphanumeric() || matches!(c, '_' | ':' | '-'));
    ok.then(|| (start, word.to_lowercase()))
}

/// `text` with each `$name` of a known skill as the reference that invokes it.
pub fn with_skill_refs(text: &str, skills: &[Skill]) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(i) = rest.find('$') {
        let lead_ok = rest[..i].chars().next_back().map_or_else(
            || out.chars().next_back().is_none_or(char::is_whitespace),
            char::is_whitespace,
        );
        out.push_str(&rest[..i]);
        let after = &rest[i + 1..];
        let len = after
            .find(|c: char| !(c.is_alphanumeric() || matches!(c, '_' | ':' | '-')))
            .unwrap_or(after.len());
        let name = after[..len].trim_end_matches(':');
        match skills
            .iter()
            .find(|s| s.name == name)
            .filter(|_| lead_ok && !name.is_empty())
        {
            Some(skill) => {
                out.push_str(&skill.reference);
                rest = &after[name.len()..];
            }
            None => {
                out.push('$');
                rest = after;
            }
        }
    }
    out.push_str(rest);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tool_lines_split_like_the_app() {
        let cases = [
            (
                "exec: cargo test → ok (running)",
                ("exec", "cargo test", "ok", ToolState::Running),
            ),
            ("read: /a/b.rs", ("read", "/a/b.rs", "", ToolState::Done)),
            (
                "web search: rune → 3 results (error)",
                ("web search", "rune", "3 results", ToolState::Failed),
            ),
            ("todo", ("todo", "", "", ToolState::Done)),
            ("codemode", ("tool", "codemode", "", ToolState::Done)),
            ("execute: x", ("tool", "execute: x", "", ToolState::Done)),
        ];
        for (line, (kind, arg, output, state)) in cases {
            assert_eq!(
                parse_tool(line),
                ToolCall {
                    kind,
                    arg,
                    output,
                    state
                },
                "{line}"
            );
        }
    }

    #[test]
    fn tool_groups_summarize_like_the_desktop() {
        let lines = [
            "exec: ls",
            "exec: pwd",
            "edit: a.rs",
            "edit: a.rs",
            "read: b.rs",
            "mcp: x/y (error)",
        ];
        let tools: Vec<_> = lines.iter().map(|l| parse_tool(l)).collect();
        assert_eq!(
            tool_summary(&tools),
            "Ran 2 commands · edited 1 file · read 1 file · called 1 tool · 1 failed"
        );
    }

    #[test]
    fn references_show_as_labels() {
        let cases: [(&str, &[(&str, bool)]); 4] = [
            (
                "run [$commit](zeron-invoke:7b22) now",
                &[("run ", false), ("$commit", true), (" now", false)],
            ),
            (
                "[a](https://x) and [b](zeron-file:/p)",
                &[("[a](https://x) and ", false), ("b", true)],
            ),
            ("no refs", &[("no refs", false)]),
            (
                "[x](zeron-chat:has space)",
                &[("[x](zeron-chat:has space)", false)],
            ),
        ];
        for (text, want) in cases {
            assert_eq!(references(text), want, "{text}");
        }
    }

    #[test]
    fn routed_messages_name_their_chat() {
        let text = "[Message from Zeron chat Fix CI. Reply to it with the Zeron `send_message` tool, chat abc]\n\nhello";
        assert_eq!(split_from(text), (Some("Fix CI"), "hello"));
        assert_eq!(split_from("hello"), (None, "hello"));
    }

    #[test]
    fn images_attach_like_the_desktop() {
        let paths = ["/u/a-Image_1.png".to_owned(), "/u/b-Image_2.png".to_owned()];
        let sent = with_images("see ", &paths);
        // As Zeron's composer sends it (seen in a live transcript).
        assert_eq!(
            sent,
            "see [Image 1](zeron-image:1) [Image 2](zeron-image:2)\n\nAttached images (local files — open them to view):\n- /u/a-Image_1.png\n- /u/b-Image_2.png"
        );
        assert_eq!(
            split_images(&sent),
            (
                "see [Image 1](zeron-image:1) [Image 2](zeron-image:2)",
                vec!["/u/a-Image_1.png", "/u/b-Image_2.png"]
            )
        );
        assert_eq!(
            with_images("", &paths[..1]).lines().next(),
            Some("[Image 1](zeron-image:1)")
        );
        assert_eq!(with_images("hi", &[]), "hi");
        assert_eq!(split_images("hi"), ("hi", vec![]));
    }

    #[test]
    fn skill_words_complete_and_expand() {
        assert_eq!(skill_query("use $com", 8), Some((4, "com".to_owned())));
        assert_eq!(skill_query("costs 5$", 8), None);
        assert_eq!(skill_query("$a b", 4), None);
        let skills = [Skill {
            name: "commit".into(),
            description: String::new(),
            reference: "[$commit](zeron-invoke:1)".into(),
        }];
        assert_eq!(
            with_skill_refs("$commit then $other, a$commit", &skills),
            "[$commit](zeron-invoke:1) then $other, a$commit"
        );
    }

    #[test]
    fn ages_read_like_the_sidebar() {
        let now: DateTime<Utc> = "2026-11-06T12:00:00Z".parse().unwrap();
        for (secs, want) in [
            (5, "now"),
            (125, "2m"),
            (7300, "2h"),
            (90_000, "1d"),
            (1_300_000, "2w"),
        ] {
            assert_eq!(ago(now - chrono::Duration::seconds(secs), now), want);
        }
    }
}
