//! How the companion words what it shows, as the phone app and the desktop
//! word it: ages, harness and model names, tool calls.

use chrono::{DateTime, Utc};

use super::message::{ToolCall, ToolState};

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

/// The desktop's project tone, by FNV-1a of the project path.
pub fn tone_index(path: &str) -> usize {
    let hash = path.bytes().fold(2_166_136_261u32, |h, b| {
        (h ^ u32::from(b)).wrapping_mul(16_777_619)
    });
    (hash % 8) as usize
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::companion::message::parse_tool;

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
