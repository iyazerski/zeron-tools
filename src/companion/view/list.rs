//! The chat list, as the phone shows it: chats grouped by project, most
//! recently active first, with a search over titles, projects and previews.

use chrono::Utc;
use rune::{El, TextStyle, Tx, col, icon, row, text};
use rune_kit::{IconButton, IconButtonVariant, Kit, Spinner, TextInput};

use super::icons;
use super::widgets::{dot, engine_note, faint, hide_button, tile, tones};
use crate::companion::app::State;
use crate::companion::data::{Chat, Status};
use crate::companion::labels::{ago, harness_name};

pub fn view(s: &State, k: &Kit) -> El {
    let header = row()
        .key("list-head")
        .w_full()
        .px(14.0)
        .pt(12.0)
        .pb(6.0)
        .center_y()
        .window_drag()
        .child(
            text("Chats")
                .font(TextStyle {
                    size: 17.0,
                    ..k.ty.body_medium
                })
                .color(k.c.text_base)
                .grow(1.0),
        )
        .child(
            IconButton::new(&icons::PLUS)
                .variant(IconButtonVariant::Ghost)
                .label("New chat")
                .help("New chat (⌘N)")
                .on_press(State::new_chat)
                .build(k),
        )
        .child(hide_button(k));
    let search = TextInput::new(&s.list.query)
        .placeholder("Search")
        .leading_icon(&icons::SEARCH)
        .clear_button()
        .on_edit(|s: &mut State| &mut s.list.query)
        .on_key(|s: &mut State, key, tx: &mut Tx| {
            // Return opens the first match.
            if key.key != rune::Key::Enter {
                return false;
            }
            let first = filtered(s).first().map(|c| c.id.clone());
            if let Some(id) = first {
                s.open_chat(id);
                tx.blur();
            }
            true
        })
        .build(k)
        .key("search");
    let mut body = col().key("rows").w_full().px(8.0).pb(12.0).gap(1.0);
    if let Some(note) = engine_note(s, k) {
        body = body.child(note.mb(8.0));
    }
    body = match &s.list.chats {
        None if s.list.error.is_some() => {
            body.child(empty(k, s.list.error.as_deref().unwrap_or_default()))
        }
        None => body.child(
            row()
                .w_full()
                .py(24.0)
                .justify_center()
                .child(Spinner::new().build(k)),
        ),
        Some(chats) => body.children(rows(s, k, chats)),
    };
    col()
        .w_full()
        .h_full()
        .child(header)
        .child(row().w_full().px(12.0).pb(8.0).child(search))
        .child(
            col()
                .key("list")
                .w_full()
                .grow(1.0)
                .min_h(0.0)
                .scroll(rune::ScrollSpec {
                    stick_bottom: false,
                })
                .child(body),
        )
}

fn empty(k: &Kit, why: &str) -> El {
    row()
        .w_full()
        .py(24.0)
        .justify_center()
        .child(text(why).font(k.ty.ui).color(k.c.text_muted).wrap())
}

/// The chats the search matches, in list order.
fn filtered(s: &State) -> Vec<&Chat> {
    let q = s.list.query.text.trim().to_lowercase();
    s.list
        .chats
        .iter()
        .flatten()
        .filter(|c| q.is_empty() || matches(c, &q))
        .collect()
}

fn matches(c: &Chat, q: &str) -> bool {
    let project = c.project.as_ref().map_or("", |p| p.name.as_str());
    let hay = [
        c.title(),
        project,
        c.last_message_preview.as_deref().unwrap_or_default(),
        c.branch.as_deref().unwrap_or_default(),
    ];
    hay.iter().any(|h| h.to_lowercase().contains(q))
}

fn rows(s: &State, k: &Kit, chats: &[Chat]) -> Vec<El> {
    let q = s.list.query.text.trim();
    if !q.is_empty() {
        let hits: Vec<El> = filtered(s)
            .into_iter()
            .map(|c| chat_row(s, k, c, true))
            .collect();
        return if hits.is_empty() {
            vec![empty(k, "No chats match your search")]
        } else {
            hits
        };
    }
    if chats.is_empty() {
        return vec![empty(k, "No chats yet")];
    }
    // Projects, most recently active first, like the sidebar.
    let mut groups: Vec<(String, Vec<&Chat>)> = Vec::new();
    for c in chats {
        let key = c
            .project
            .as_ref()
            .map_or_else(|| "home".to_owned(), |p| p.id.clone());
        match groups.iter_mut().find(|(k, _)| *k == key) {
            Some((_, items)) => items.push(c),
            None => groups.push((key, vec![c])),
        }
    }
    let mut out = Vec::new();
    for (key, items) in groups {
        let collapsed = s.prefs.collapsed.contains(&key);
        out.push(section(k, &key, &items, collapsed));
        if !collapsed {
            out.extend(items.into_iter().map(|c| chat_row(s, k, c, false)));
        }
    }
    out
}

fn section(k: &Kit, key: &str, items: &[&Chat], collapsed: bool) -> El {
    let project = items[0].project.as_ref();
    let live = items.iter().any(|c| c.is_working());
    let toggle = key.to_owned();
    let mut head = row()
        .key(format!("section-{key}"))
        .w_full()
        .gap(8.0)
        .px(6.0)
        .pt(10.0)
        .pb(4.0)
        .center_y()
        .child(tile(k, project, 16.0))
        .child(
            text(project.map_or("No project", |p| p.name.as_str()))
                .font(k.ty.small_medium)
                .color(k.c.text_muted)
                .truncate(),
        )
        .child(
            text(items.len().to_string())
                .font(k.ty.small)
                .color(k.c.text_faint),
        );
    if live {
        head = head.child(Spinner::new().size(10.0).build(k));
    }
    let chevron = icon(&icons::CHEV_DOWN, 14.0)
        .color(k.c.icon_faint)
        .rotate(if collapsed {
            -std::f32::consts::FRAC_PI_2
        } else {
            0.0
        });
    head.child(rune::spacer())
        .child(chevron)
        .on_press(move |s: &mut State| s.prefs.toggle_collapsed(&toggle))
}

fn chat_row(s: &State, k: &Kit, c: &Chat, with_project: bool) -> El {
    let t = tones(k);
    let status = c.status;
    let unseen = status == Status::Idle && s.prefs.unseen(c);
    let corner = match status {
        Status::Working { stale } => row()
            .gap(5.0)
            .center_y()
            .child(Spinner::new().size(10.0).build(k))
            .child(
                text(if stale { "Offline?" } else { "Working" })
                    .font(k.ty.small)
                    .color(t.working),
            ),
        Status::Input => labelled(k, dot(t.input), "Input", t.input),
        Status::Failed => labelled(k, dot(t.failed), "Failed", t.failed),
        Status::Idle if unseen => {
            labelled(k, icon(&icons::CHECK, 12.0).color(t.done), "Done", t.done)
        }
        Status::Idle => row().child(
            text(ago(c.active_at(), Utc::now()))
                .font(k.ty.small)
                .color(k.c.text_muted),
        ),
    };
    let harness = c.harness.as_deref().unwrap_or_default();
    let weight = if unseen { k.ty.ui_medium } else { k.ty.ui };
    let top = row()
        .w_full()
        .gap(8.0)
        .center_y()
        .child(
            icon(icons::mark(harness), 14.0)
                .color(k.c.icon_muted)
                .help(harness_name(harness)),
        )
        .child(
            text(c.title())
                .font(weight)
                .color(k.c.text_base)
                .truncate()
                .grow(1.0)
                .min_w(0.0),
        )
        .child(corner);
    let mut item = rune::col().w_full().gap(3.0);
    item = item.child(top);
    if with_project || c.branch.is_some() {
        let mut meta = row().w_full().gap(10.0).pl(22.0).center_y();
        if with_project {
            meta = meta.child(
                row()
                    .gap(5.0)
                    .center_y()
                    .child(tile(k, c.project.as_ref(), 12.0))
                    .child(faint(
                        k,
                        c.project.as_ref().map_or("No project", |p| p.name.as_str()),
                    )),
            );
        }
        if let Some(branch) = &c.branch {
            meta = meta.child(
                row()
                    .gap(4.0)
                    .center_y()
                    .min_w(0.0)
                    .child(icon(&icons::BRANCH, 11.0).color(k.c.icon_faint))
                    .child(faint(k, branch.as_str())),
            );
        }
        item = item.child(meta);
    }
    let id = c.id.clone();
    k.interactive(
        item.key(format!("chat-{}", c.id))
            .px(8.0)
            .py(7.0)
            .radius(8.0),
    )
    .on_press(move |s: &mut State| s.open_chat(id.clone()))
}

fn labelled(k: &Kit, lead: El, label: &str, color: rune::Color) -> El {
    row()
        .gap(5.0)
        .center_y()
        .child(lead)
        .child(text(label.to_owned()).font(k.ty.small).color(color))
}
