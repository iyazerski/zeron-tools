//! Pick chats to archive so the sidebar shows only recent work.
//!
//! Zeron's sidebar draws a row for every unarchived top-level chat, with no
//! paging (`crates/ui/src/state.rs::visible_chats`), while archived chats
//! live on a paged shelf that can be hidden. Archiving all but the most
//! recent chats of each project keeps the list short.

use std::collections::{HashMap, HashSet};

use anyhow::Result;
use chrono::{DateTime, Utc};
use zeron_proto::{Chat, Space, view::project_key};

use crate::export::last_activity;
use crate::zeron::Profile;
use crate::{Tally, date, find_space, one_line, short_id};

/// Archive [`stale`] chats (only those of `project`, if given; a name or
/// folder), skipping pinned and sectioned ones, and print a per-project
/// summary.
pub fn run(
    profile: &mut Profile,
    keep: usize,
    idle_before: Option<DateTime<Utc>>,
    project: Option<&str>,
    dry_run: bool,
) -> Result<Tally> {
    let every = profile.chats()?;
    let spaces = profile.spaces()?;
    let space = project.map(|p| find_space(&spaces, p)).transpose()?;
    let placed = profile.sidebar_placed_chats();
    let protected: HashSet<&str> = placed.iter().map(String::as_str).collect();
    let chosen: Vec<_> = stale(&every, &spaces, keep, idle_before, &protected)
        .into_iter()
        .filter(|c| space.is_none_or(|s| c.space_id.as_deref() == Some(s.id.as_str())))
        .collect();
    let verb = if dry_run { "would archive" } else { "archived" };
    let mut per_project: HashMap<&str, usize> = HashMap::new();
    for chat in &chosen {
        let name = spaces
            .iter()
            .find(|s| Some(&s.id) == chat.space_id.as_ref())
            .map_or("no project", |s| s.display_name());
        *per_project.entry(name).or_default() += 1;
        println!(
            "{verb} {}  {}  {name}  {}",
            short_id(&chat.id),
            date(last_activity(chat).timestamp_millis()),
            one_line(chat.title.as_deref().unwrap_or("Untitled"), 50)
        );
        if !dry_run {
            profile.set_archived(&chat.id, true)?;
        }
    }
    profile.save()?;
    let mut summary: Vec<_> = per_project.into_iter().collect();
    summary.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(b.0)));
    let active = every
        .iter()
        .filter(|c| !c.archived && c.is_top_level())
        .count();
    eprintln!(
        "{verb} {} of {active} sidebar chats{}",
        chosen.len(),
        summary
            .iter()
            .map(|(p, n)| format!("\n  {n:>4}  {p}"))
            .collect::<String>()
    );
    Ok(Tally {
        done: chosen.len(),
        failed: 0,
    })
}

/// Unarchived top-level chats beyond the `keep` most recently active in their
/// project (project-less chats form one group), optionally only those idle
/// since before `idle_before`. Chats in `protected` (pinned or placed in a
/// sidebar section) never count as stale and don't use up `keep`.
pub fn stale<'a>(
    chats: &'a [Chat],
    spaces: &[Space],
    keep: usize,
    idle_before: Option<DateTime<Utc>>,
    protected: &HashSet<&str>,
) -> Vec<&'a Chat> {
    // Group like the sidebar does: every checkout of one repository is one project.
    let project: HashMap<&str, String> = spaces
        .iter()
        .map(|s| (s.id.as_str(), project_key(s)))
        .collect();
    let mut groups: HashMap<Option<&str>, Vec<&Chat>> = HashMap::new();
    for chat in chats {
        if chat.archived || !chat.is_top_level() || protected.contains(chat.id.as_str()) {
            continue;
        }
        let key = chat
            .space_id
            .as_deref()
            .map(|id| project.get(id).map_or(id, String::as_str));
        groups.entry(key).or_default().push(chat);
    }
    let mut out = vec![];
    for mut group in groups.into_values() {
        // The sidebar's "last updated" order, newest first.
        group.sort_by(|a, b| {
            last_activity(b)
                .cmp(&last_activity(a))
                .then_with(|| a.id.cmp(&b.id))
        });
        out.extend(
            group
                .into_iter()
                .skip(keep)
                .filter(|c| idle_before.is_none_or(|t| last_activity(c) < t)),
        );
    }
    out.sort_by_key(|c| last_activity(c));
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn chat(id: &str, space: Option<&str>, day: u32) -> Chat {
        serde_json::from_value(serde_json::json!({
            "id": id,
            "deviceId": "d",
            "archived": false,
            "createdAt": "2026-09-01T00:00:00Z",
            "lastMessageAt": format!("2026-09-{day:02}T00:00:00Z"),
            "spaceId": space,
        }))
        .unwrap()
    }

    fn space(id: &str, repo: Option<&str>) -> Space {
        serde_json::from_value(serde_json::json!({
            "id": id,
            "deviceId": "d",
            "path": format!("/w/{id}"),
            "createdAt": "2026-09-01T00:00:00Z",
            "gitDetected": true,
            "repositoryId": repo,
        }))
        .unwrap()
    }

    fn ids(chats: Vec<&Chat>) -> Vec<&str> {
        chats.into_iter().map(|c| c.id.as_str()).collect()
    }

    #[test]
    fn keeps_the_most_recent_chats_of_each_project() {
        let spaces = [
            space("a", Some("r")),
            space("a2", Some("r")),
            space("b", None),
        ];
        let mut archived = chat("archived", Some("a"), 1);
        archived.archived = true;
        let mut child = chat("child", Some("a"), 1);
        child.parent_chat_id = Some("a-new".into());
        let chats = [
            chat("a-old", Some("a"), 2),
            chat("a2-mid", Some("a2"), 5), // another checkout of the same repo
            chat("a-new", Some("a"), 9),
            chat("b-old", Some("b"), 3),
            chat("b-new", Some("b"), 4),
            chat("none-old", None, 1),
            chat("none-new", None, 8),
            chat("pinned", Some("a"), 1),
            archived,
            child,
        ];
        let pinned = HashSet::from(["pinned"]);
        assert_eq!(
            ids(stale(&chats, &spaces, 1, None, &pinned)),
            ["none-old", "a-old", "b-old", "a2-mid"]
        );
        let idle_before = "2026-09-03T00:00:00Z".parse().ok();
        assert_eq!(
            ids(stale(&chats, &spaces, 1, idle_before, &pinned)),
            ["none-old", "a-old"]
        );
        assert!(stale(&chats, &spaces, 2, None, &pinned).len() == 1);
    }
}
