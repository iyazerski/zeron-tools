//! A chat's page, as the phone shows it: the header, the transcript with its
//! tool groups and errors, the agent's questions, and the composer under them.

use rune::{El, ScrollSpec, Span, TextStyle, Tx, col, icon, rich, row, text};
use rune_kit::{
    Button, ButtonVariant, IconButton, IconButtonVariant, Kit, Markdown, Menu, MenuItem, Spinner,
    TextInput,
};

use super::app::{ChatPage, QuestionForm, State};
use super::data::{
    Message, Part, Role, ToolState, harness_name, parse_tool, references, short_model, split_from,
    split_images, tool_summary, tool_verb,
};
use super::icons;
use super::ui::{dot, engine_note, tones};

pub fn view(s: &State, page: &ChatPage, k: &Kit, layer: &mut Vec<El>) -> El {
    let mut body = col().key("transcript").w_full().px(14.0).py(10.0).gap(14.0);
    if let Some(note) = engine_note(s, k) {
        body = body.child(note);
    } else if let Some(e) = &page.error {
        body = body.child(errbox(k, e));
    }
    if page.draft.is_some() {
        body = body.child(hero(k));
    } else {
        body = body.children(transcript(page, k));
    }
    let scroll = col()
        .key("transcript")
        .w_full()
        .grow(1.0)
        .min_h(0.0)
        .scroll(ScrollSpec { stick_bottom: true })
        .scroll_anchor()
        .on_far_from_bottom(160.0, |s: &mut State, far| {
            if let Some(page) = s.chat_mut() {
                page.far_from_bottom = far;
            }
        })
        .child(body.key("messages"));
    let mut stack = rune::el().w_full().grow(1.0).min_h(0.0).child(scroll);
    if page.far_from_bottom {
        let jump = IconButton::new(&icons::CHEV_DOWN)
            .variant(IconButtonVariant::Neutral)
            .label("Scroll to bottom")
            .on_press_tx(|_: &mut State, tx: &mut Tx| tx.scroll_to_bottom("transcript"))
            .build(k);
        stack = stack.child(
            row()
                .pin(None, None, Some(0.0), Some(10.0))
                .w_full()
                .justify_center()
                .child(jump),
        );
    }
    col()
        .w_full()
        .h_full()
        .child(header(s, page, k, layer))
        .child(stack)
        .child(super::composer::view(s, page, k, layer))
}

fn header(s: &State, page: &ChatPage, k: &Kit, layer: &mut Vec<El>) -> El {
    let (title, sub) = match (&page.draft, &page.chat) {
        (Some(d), _) => {
            let project = d
                .project
                .as_ref()
                .and_then(|id| d.projects.iter().find(|p| &p.id == id));
            let parts = [
                d.harness.as_deref().map(harness_name),
                project.map(|p| p.name.as_str()),
            ];
            (
                "New chat".to_owned(),
                parts.into_iter().flatten().collect::<Vec<_>>().join(" · "),
            )
        }
        (None, Some(c)) => {
            let parts = [
                c.harness.as_deref().map(harness_name),
                c.model.as_deref().map(short_model),
                c.project.as_ref().map(|p| p.name.as_str()),
            ];
            (
                c.title().to_owned(),
                parts.into_iter().flatten().collect::<Vec<_>>().join(" · "),
            )
        }
        (None, None) => (String::new(), String::new()),
    };
    let back = IconButton::new(&icons::BACK)
        .variant(IconButtonVariant::Ghost)
        .label("Back")
        .help("Back (⌘[)")
        .on_press(State::back)
        .build(k);
    let titles = col()
        .grow(1.0)
        .min_w(0.0)
        .gap(1.0)
        .child(
            text(title)
                .font(k.ty.ui_medium)
                .color(k.c.text_base)
                .truncate(),
        )
        .child(
            text(sub)
                .font(k.ty.text_xsmall)
                .color(k.c.text_faint)
                .truncate(),
        );
    let mut bar = row()
        .key("chat-head")
        .w_full()
        .px(8.0)
        .pt(8.0)
        .pb(6.0)
        .gap(6.0)
        .center_y()
        .window_drag()
        .child(back)
        .child(titles);
    if page.draft.is_none() {
        bar = bar.child(more_menu(s, page, k, layer));
    }
    bar = bar.child(super::ui::hide_button(k));
    bar
}

fn more_menu(_: &State, page: &ChatPage, k: &Kit, layer: &mut Vec<El>) -> El {
    let trigger = IconButton::new(&icons::MORE)
        .variant(IconButtonVariant::Ghost)
        .label("More")
        .expanded(page.menu)
        .on_press(|s: &mut State| {
            if let Some(p) = s.chat_mut() {
                p.menu = !p.menu;
            }
        })
        .build(k);
    let mut menu = Menu::new("chat-menu")
        .trigger(trigger)
        .open(page.menu)
        .on_close(close_menu);
    if page
        .chat
        .as_ref()
        .is_some_and(super::data::Chat::is_working)
    {
        menu = menu.item(
            MenuItem::new("Stop the agent")
                .icon(&icons::STOP)
                .danger(true)
                .on_select(|s: &mut State| {
                    close_menu(s);
                    s.stop();
                }),
        );
    }
    menu.item(
        MenuItem::new("Restart Zeron")
            .icon(&icons::RESTART)
            .description("If it hangs")
            .on_select(|s: &mut State| {
                close_menu(s);
                s.restart_zeron();
            }),
    )
    .item(
        MenuItem::new("Archive")
            .icon(&icons::ARCHIVE)
            .danger(true)
            .on_select(|s: &mut State| {
                close_menu(s);
                s.archive();
            }),
    )
    .build(k, layer)
}

fn close_menu(s: &mut State) {
    if let Some(p) = s.chat_mut() {
        p.menu = false;
    }
}

fn hero(k: &Kit) -> El {
    col().w_full().pt(80.0).items_center().child(
        text("What are we building?")
            .font(TextStyle {
                size: 20.0,
                ..k.ty.body_medium
            })
            .color(k.c.text_muted),
    )
}

fn errbox(k: &Kit, why: &str) -> El {
    row()
        .w_full()
        .gap(8.0)
        .padding(10.0)
        .radius(10.0)
        .fill(k.c.danger.bg)
        .child(icon(&icons::ALERT, 14.0).color(k.c.danger.icon))
        .child(
            text(why.to_owned())
                .font(k.ty.ui)
                .color(k.c.text_base)
                .wrap()
                .grow(1.0)
                .min_w(0.0),
        )
}

fn transcript(page: &ChatPage, k: &Kit) -> Vec<El> {
    let Some(read) = &page.read else {
        return vec![
            row()
                .w_full()
                .py(32.0)
                .justify_center()
                .child(Spinner::new().build(k)),
        ];
    };
    let working = page
        .chat
        .as_ref()
        .is_some_and(super::data::Chat::is_working);
    let mut out = Vec::new();
    if read.older_remaining > 0 {
        out.push(
            row().w_full().justify_center().child(
                Button::new("Load earlier messages")
                    .variant(ButtonVariant::Ghost)
                    .on_press(State::load_earlier)
                    .build(k),
            ),
        );
    }
    let last = read.messages.last().map(|m| m.id.as_str());
    for m in &read.messages {
        match m.role {
            Role::User => {
                out.push(user_bubble(k, page, &m.text(), false).key(format!("m-{}", m.id)))
            }
            Role::Assistant => {
                let streaming = m.status.as_deref() == Some("streaming")
                    || (Some(m.id.as_str()) == last && working);
                out.push(assistant(page, k, m, streaming).key(format!("m-{}", m.id)));
            }
            Role::System => {}
        }
    }
    for (i, text) in page.pending.iter().enumerate() {
        out.push(user_bubble(k, page, text, true).key(format!("pending-{i}")));
    }
    if working && quiet(read.messages.last()) {
        let since = read
            .messages
            .iter()
            .rev()
            .find(|m| m.role == Role::User)
            .map(|m| m.created_at);
        let secs = since.map(|t| (chrono::Utc::now().timestamp_millis() - t).max(0) / 1000);
        out.push(
            row()
                .key("working")
                .gap(8.0)
                .center_y()
                .child(Spinner::new().size(12.0).build(k))
                .child(text("Working").font(k.ty.ui).color(k.c.text_muted))
                .child(
                    text(secs.map(|s| format!("{s}s")).unwrap_or_default())
                        .font(k.ty.ui)
                        .color(k.c.text_faint),
                ),
        );
    }
    if let Some(form) = &page.question {
        out.push(question(k, form).key(format!("q-{}", form.input.request_id)));
    }
    out
}

/// Whether the agent shows nothing yet: no prose and no running tool.
fn quiet(last: Option<&Message>) -> bool {
    last.is_none_or(|m| {
        m.role == Role::User
            || !m.parts.iter().any(|p| match p {
                Part::Text { .. } => true,
                Part::Tools { tools } => tools.iter().any(|t| t.ends_with("(running)")),
                Part::Error { .. } => false,
            })
    })
}

fn user_bubble(k: &Kit, page: &ChatPage, text_in: &str, pending: bool) -> El {
    let (from, body) = split_from(text_in);
    let (body, images) = split_images(body);
    // Composer references become labels; the rest stays literal text.
    let mut shown = String::new();
    let mut spans = Vec::new();
    for (piece, is_ref) in references(body) {
        let start = shown.len();
        shown.push_str(piece);
        if is_ref {
            spans.push(
                Span::new(start, shown.len())
                    .color(k.c.text_accent)
                    .weight(560.0),
            );
        }
    }
    let bubble = col()
        .shrink()
        .min_w(0.0)
        .px(12.0)
        .py(8.0)
        .radius(14.0)
        .fill(k.c.layer_03)
        .opacity(if pending { 0.6 } else { 1.0 })
        .child(
            rich(shown, spans)
                .font(k.ty.ui)
                .color(k.c.text_base)
                .selectable(true),
        );
    let mut out = col().w_full().pl(40.0).items_end().gap(4.0);
    if let Some(from) = from {
        out = out.child(
            text(format!("From chat {from}"))
                .font(k.ty.text_xsmall)
                .color(k.c.text_faint),
        );
    }
    if !images.is_empty() {
        out = out.child(row().gap(6.0).children(images.into_iter().map(|path| {
            let url = format!("file://{path}");
            k.interactive(super::composer::thumb(k, page.thumbs.get(path), 72.0))
                .key(format!("img-{path}"))
                .label("Open the image")
                .opacity(if pending { 0.6 } else { 1.0 })
                .on_press_tx(move |_: &mut State, tx: &mut Tx| drop(tx.open_url(&url)))
        })));
    }
    out.child(bubble)
}

/// Prose, tool groups and errors in transcript order, as on the desktop.
fn assistant(page: &ChatPage, k: &Kit, m: &Message, streaming: bool) -> El {
    let mut out = col().w_full().gap(10.0);
    for (i, part) in m.parts.iter().enumerate() {
        out = out.child(match part {
            Part::Text { text } => Markdown::new(text.clone())
                .streaming(streaming && i + 1 == m.parts.len())
                .selectable(true)
                .build(k)
                .key(format!("{}#{i}", m.id)),
            Part::Tools { tools } => tool_group(page, k, &format!("{}#{i}", m.id), tools),
            Part::Error { error } => errbox(k, error),
        });
    }
    if m.status.as_deref() == Some("aborted") {
        out = out.child(text("Stopped").font(k.ty.small).color(k.c.text_faint));
    }
    out
}

fn tool_group(page: &ChatPage, k: &Kit, key: &str, lines: &[String]) -> El {
    let tools: Vec<_> = lines.iter().map(|l| parse_tool(l)).collect();
    let open = page.open_groups.contains(key);
    let live = tools.iter().any(|t| t.state == ToolState::Running);
    let toggle = key.to_owned();
    let mut head = row().w_full().gap(6.0).center_y();
    if live {
        head = head.child(Spinner::new().size(11.0).build(k));
    }
    head = head
        .child(
            text(tool_summary(&tools))
                .font(k.ty.small)
                .color(k.c.text_muted)
                .truncate()
                .min_w(0.0),
        )
        .child(
            icon(&icons::CHEV_DOWN, 13.0)
                .color(k.c.icon_faint)
                .rotate(if open {
                    0.0
                } else {
                    -std::f32::consts::FRAC_PI_2
                }),
        );
    let head = k
        .interactive(head.px(6.0).py(4.0).radius(6.0))
        .on_press(move |s: &mut State| {
            if let Some(p) = s.chat_mut()
                && !p.open_groups.remove(&toggle)
            {
                p.open_groups.insert(toggle.clone());
            }
        });
    let mut group = col()
        .key(format!("tools-{key}"))
        .w_full()
        .gap(2.0)
        .child(head);
    if open {
        let t = tones(k);
        let mut rail = col()
            .w_full()
            .gap(4.0)
            .pl(12.0)
            .ml(6.0)
            .border(0.0, k.c.border_muted);
        for tool in &tools {
            let lead = match tool.state {
                ToolState::Running => Spinner::new().size(11.0).build(k),
                _ => icon(icons::tool(tool.kind), 12.0).color(if tool.state == ToolState::Failed {
                    t.failed
                } else {
                    k.c.icon_faint
                }),
            };
            let arg = if tool.kind == "mcp" {
                tool.arg.rsplit('/').next().unwrap_or(tool.arg)
            } else {
                tool.arg
            };
            let mut line = col().grow(1.0).min_w(0.0).child(
                row()
                    .gap(5.0)
                    .child(
                        text(tool_verb(tool.kind))
                            .font(k.ty.small_medium)
                            .color(k.c.text_muted),
                    )
                    .child(
                        text(arg.to_owned())
                            .font(TextStyle {
                                size: 11.5,
                                ..k.ty.mono
                            })
                            .color(k.c.text_muted)
                            .truncate()
                            .min_w(0.0),
                    ),
            );
            if !tool.output.is_empty() {
                line = line.child(
                    text(tool.output.to_owned())
                        .font(TextStyle {
                            size: 11.0,
                            ..k.ty.mono
                        })
                        .color(k.c.text_faint)
                        .truncate(),
                );
            }
            rail = rail.child(
                row()
                    .w_full()
                    .gap(6.0)
                    .items_start()
                    .child(lead.mt(2.0))
                    .child(line),
            );
        }
        group = group.child(rail);
    }
    group
}

/// The agent is blocked on questions: pick options, type an answer, or both.
fn question(k: &Kit, form: &QuestionForm) -> El {
    let t = tones(k);
    let mut card = col()
        .w_full()
        .gap(10.0)
        .padding(12.0)
        .radius(12.0)
        .fill(k.c.layer_02)
        .border(1.0, k.c.border_base)
        .child(
            row().gap(6.0).center_y().child(dot(t.input)).child(
                text("Needs your answer")
                    .font(k.ty.ui_medium)
                    .color(k.c.text_base),
            ),
        );
    for (qi, q) in form.input.questions.iter().enumerate() {
        let label = if q.header.is_empty() {
            q.question.clone()
        } else {
            format!("{}: {}", q.header, q.question)
        };
        card = card.child(text(label).font(k.ty.ui).color(k.c.text_base).wrap());
        for option in &q.options {
            let on = form.picked[qi].contains(option);
            let (option_key, multi) = (option.clone(), q.multi_select);
            let check = rune::el()
                .size(14.0, 14.0)
                .radius(if multi { 3.0 } else { 7.0 })
                .border(1.0, k.c.border_strong)
                .center()
                .child(if on {
                    icon(&icons::CHECK, 11.0).color(k.c.text_base)
                } else {
                    rune::el()
                });
            let opt = row()
                .w_full()
                .gap(8.0)
                .px(8.0)
                .py(6.0)
                .radius(8.0)
                .center_y()
                .fill(if on { k.c.layer_04 } else { k.c.layer_01 })
                .child(check)
                .child(
                    text(option.clone())
                        .font(k.ty.ui)
                        .color(k.c.text_base)
                        .wrap()
                        .grow(1.0)
                        .min_w(0.0),
                );
            card = card.child(
                k.interactive(opt)
                    .key(format!("opt-{qi}-{option}"))
                    .on_press(move |s: &mut State| {
                        let Some(form) = s.chat_mut().and_then(|p| p.question.as_mut()) else {
                            return;
                        };
                        let picked = &mut form.picked[qi];
                        if !picked.remove(&option_key) {
                            if !multi {
                                picked.clear();
                            }
                            picked.insert(option_key.clone());
                        }
                    }),
            );
        }
        let field = TextInput::new(&form.fields[qi])
            .placeholder(if q.options.is_empty() {
                "Your answer"
            } else {
                "Other…"
            })
            .on_edit(move |s: &mut State| s.answer_field(qi))
            .build(k)
            .key(format!("answer-{qi}"));
        card = card.child(field);
    }
    card.child(
        Button::new("Submit")
            .variant(ButtonVariant::Contrast)
            .disabled(form.sending)
            .on_press(State::respond)
            .build(k),
    )
}
