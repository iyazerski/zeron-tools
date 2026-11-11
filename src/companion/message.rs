//! What a message carries besides its prose, as Zeron's composer writes it:
//! tool call lines, attached images, routing headers, and references.

use super::data::Skill;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolState {
    Done,
    Running,
    Failed,
}

/// One tool call line (`exec: cargo test (running)`), split up.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolCall<'a> {
    /// `exec`, `read`, … or `tool` for one the app doesn't name.
    pub kind: &'a str,
    pub arg: &'a str,
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
    let named = TOOL_KINDS.iter().find_map(|kind| {
        let rest = text.strip_prefix(kind)?;
        if rest.is_empty() {
            return Some((*kind, ""));
        }
        rest.strip_prefix(':')
            .map(|arg| (*kind, arg.strip_prefix(' ').unwrap_or(arg)))
    });
    let (kind, arg) = named.unwrap_or(("tool", text));
    ToolCall { kind, arg, state }
}

/// What Zeron's composer appends to a message with images: the agent opens
/// them by path, and the transcript finds them there.
const IMAGES_HEAD: &str = "\n\nAttached images (local files — open them to view):";

/// The message Zeron's composer sends for `text` with images saved at
/// `paths`: a `zeron-image` reference per image, then their paths.
pub fn with_images(mut out: String, paths: &[String]) -> String {
    if paths.is_empty() {
        return out;
    }
    out.truncate(out.trim_end().len());
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
                "exec: cargo test (running)",
                ("exec", "cargo test", ToolState::Running),
            ),
            ("read: /a/b.rs", ("read", "/a/b.rs", ToolState::Done)),
            (
                "web search: rune (error)",
                ("web search", "rune", ToolState::Failed),
            ),
            ("todo", ("todo", "", ToolState::Done)),
            ("codemode", ("tool", "codemode", ToolState::Done)),
            ("execute: x", ("tool", "execute: x", ToolState::Done)),
        ];
        for (line, (kind, arg, state)) in cases {
            assert_eq!(parse_tool(line), ToolCall { kind, arg, state }, "{line}");
        }
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
        let sent = with_images("see ".into(), &paths);
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
            with_images(String::new(), &paths[..1]).lines().next(),
            Some("[Image 1](zeron-image:1)")
        );
        assert_eq!(with_images("hi".into(), &[]), "hi");
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
}
