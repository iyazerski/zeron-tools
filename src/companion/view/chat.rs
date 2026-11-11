//! A chat's page, as the phone shows it: the header, the transcript with its
//! tool groups and errors, the agent's questions, and the composer under them.

use rune::{Color, El, ScrollSpec, Span, TextStyle, Tx, col, icon, rich, row, text};
use rune_kit::markdown::MarkdownStyle;
use rune_kit::{
    Button, ButtonVariant, ControlSize, IconButton, IconButtonVariant, Kit, Markdown, Menu,
    MenuItem, TextInput,
};

use super::{code, icons};

use super::widgets::{busy, dot, engine_note, head, head_titles, hide_button, tones};
use crate::companion::app::{ChatPage, Existing, Overlay, QuestionForm, State, Target};
use crate::companion::data::{Message, MessageStatus, Part, Role};
use crate::companion::labels::{tool_summary, tool_verb};
use crate::companion::message::{
    ToolCall, ToolState, parse_tool, references, split_from, split_images,
};
use rune_theme_opencode::file_icons;

pub fn view(s: &State, page: &ChatPage, k: &Kit, layer: &mut Vec<El>) -> El {
    if let Some(chat) = page.existing()
        && let Some(changes) = &chat.changes
    {
        return super::changes::page(s, chat, changes, k);
    }
    let mut body = col().key("transcript").w_full().px(14.0).py(10.0).gap(14.0);
    let error = page.existing().and_then(|c| c.error.as_deref());
    if let Some(note) = engine_note(s, k) {
        body = body.child(note);
    } else if let Some(e) = error {
        body = body.child(errbox(k, e));
    }
    body = match &page.target {
        Target::New(_) => body.child(hero(k)),
        Target::Existing(chat) => body.children(transcript(chat, k)),
    };
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
        .child(header(page, k, layer))
        .child(stack)
        .child(super::composer::view(s, page, k, layer))
}

fn header(page: &ChatPage, k: &Kit, layer: &mut Vec<El>) -> El {
    let (title, sub) = match &page.target {
        Target::New(d) => {
            let project = d
                .project
                .as_ref()
                .and_then(|id| d.projects.iter().find(|p| &p.id == id));
            (
                "New chat".to_owned(),
                project.map(|p| p.name.clone()).unwrap_or_default(),
            )
        }
        Target::Existing(Existing { chat: Some(c), .. }) => (
            c.title().to_owned(),
            c.project
                .as_ref()
                .map(|p| p.name.clone())
                .unwrap_or_default(),
        ),
        Target::Existing(_) => (String::new(), String::new()),
    };
    let back = IconButton::new(&icons::BACK)
        .variant(IconButtonVariant::Ghost)
        .label("Back")
        .help("Back (⌘[)")
        .on_press(State::back)
        .build(k);
    let titles = head_titles()
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
    let mut bar = head("chat-head").child(back).child(titles);
    if page.existing().is_some() {
        bar = bar.child(more_menu(page, k, layer));
    }
    bar = bar.child(hide_button(k));
    bar
}

fn more_menu(page: &ChatPage, k: &Kit, layer: &mut Vec<El>) -> El {
    let trigger = IconButton::new(&icons::MORE)
        .variant(IconButtonVariant::Ghost)
        .label("More")
        .expanded(page.overlay == Overlay::Menu)
        .on_press(|s: &mut State| {
            if let Some(p) = s.chat_mut() {
                p.overlay = match p.overlay {
                    Overlay::Menu => Overlay::Closed,
                    Overlay::Closed | Overlay::Sheet(_) => Overlay::Menu,
                };
            }
        })
        .build(k);
    let mut menu = Menu::new("chat-menu")
        .trigger(trigger)
        .open(page.overlay == Overlay::Menu)
        .on_close(close_menu);
    if page.is_working() {
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
    if let Some(p) = s.chat_mut().filter(|p| p.overlay == Overlay::Menu) {
        p.overlay = Overlay::Closed;
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

fn transcript(page: &Existing, k: &Kit) -> Vec<El> {
    let Some(read) = &page.read else {
        return vec![
            row()
                .w_full()
                .py(32.0)
                .justify_center()
                .child(busy(18.0, k)),
        ];
    };
    let working = page.chat.as_ref().is_some_and(|c| c.is_working());
    let style = code::markdown_style(k);
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
                let streaming = m.status == Some(MessageStatus::Streaming)
                    || (Some(m.id.as_str()) == last && working);
                out.push(assistant(page, k, &style, m, streaming).key(format!("m-{}", m.id)));
            }
            Role::System => {}
        }
    }
    for (i, text) in page.pending.iter().enumerate() {
        out.push(user_bubble(k, page, text, true).key(format!("pending-{i}")));
    }
    if page.shows_clock() {
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
                .child(busy(12.0, k))
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
    } else if let Some(counts) = read.turn_changes.filter(|_| !working) {
        out.push(super::changes::button(k, counts));
    }
    out
}

fn user_bubble(k: &Kit, page: &Existing, text_in: &str, pending: bool) -> El {
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
            k.interactive(super::composer::thumb(
                k,
                page.thumbs.get(path).and_then(Option::as_ref),
                72.0,
            ))
            .key(format!("img-{path}"))
            .label("Open the image")
            .opacity(if pending { 0.6 } else { 1.0 })
            .on_press_tx(move |_: &mut State, tx: &mut Tx| drop(tx.open_url(&url)))
        })));
    }
    out.child(bubble)
}

/// Prose, tool groups and errors in transcript order, as on the desktop.
fn assistant(page: &Existing, k: &Kit, style: &MarkdownStyle, m: &Message, streaming: bool) -> El {
    let mut out = col().w_full().gap(10.0);
    for (i, part) in m.parts.iter().enumerate() {
        out = out.child(match part {
            Part::Text { text } => Markdown::new(text.clone())
                .style(style.clone())
                .highlights(code::inline_code_marks(k, text))
                .streaming(streaming && i + 1 == m.parts.len())
                .selectable(true)
                .copy_source(true)
                .on_link_tx(|_: &mut State, url: &str, tx: &mut Tx| drop(tx.open_url(url)))
                .build(k)
                .key(format!("{}#{i}", m.id)),
            Part::Tools { tools } => tool_group(page, k, &format!("{}#{i}", m.id), tools),
            Part::Error { error } => errbox(k, error),
        });
    }
    if m.status == Some(MessageStatus::Aborted) {
        out = out.child(text("Stopped").font(k.ty.small).color(k.c.text_faint));
    }
    if !streaming {
        out = out.child(footer(page, k, m));
    }
    out
}

/// When the message started, and a button that copies its closing answer as
/// Markdown, as the desktop shows under a finished reply.
fn footer(page: &Existing, k: &Kit, m: &Message) -> El {
    let mut out = row().gap(8.0).center_y();
    if let Some(at) = chrono::DateTime::from_timestamp_millis(m.created_at) {
        out = out.child(
            text(
                at.with_timezone(&chrono::Local)
                    .format("%b %-d, %-I:%M %p")
                    .to_string(),
            )
            .font(k.ty.small)
            .color(k.c.text_faint),
        );
    }
    if m.has_answer() {
        let copied = page.just_copied(&m.id);
        let id = m.id.clone();
        out = out.child(
            IconButton::new(if copied { &icons::CHECK } else { &icons::COPY })
                .variant(IconButtonVariant::GhostMuted)
                .control_size(ControlSize::Small)
                .label(if copied { "Copied" } else { "Copy" })
                .help("Copy the answer")
                .on_press_tx(move |s: &mut State, tx: &mut Tx| s.copy_answer(&id, tx))
                .build(k),
        );
    }
    out
}

fn tool_group(page: &Existing, k: &Kit, key: &str, lines: &[String]) -> El {
    let tools: Vec<_> = lines.iter().map(|l| parse_tool(l)).collect();
    let open = page.open_groups.contains(key);
    let live = tools.iter().any(|t| t.state == ToolState::Running);
    let toggle = key.to_owned();
    // The chevron sits on the rail's trunk, the summary over the rows' icons.
    let mut head = row()
        .h(TOOL_HEADER_HEIGHT)
        .gap(RAIL_X)
        .pr(6.0)
        .center_y()
        .min_w(0.0)
        .child(
            icon(&icons::CHEV_DOWN, RAIL_X * 2.0)
                .color(k.c.icon_faint)
                .rotate(if open {
                    0.0
                } else {
                    -std::f32::consts::FRAC_PI_2
                }),
        )
        .child(
            text(tool_summary(&tools))
                .font(TextStyle {
                    size: TOOL_TEXT,
                    ..k.ty.ui
                })
                .color(k.c.text_muted)
                .truncate()
                .min_w(0.0),
        );
    if live {
        head = head.child(busy(11.0, k));
    }
    let head = k
        .interactive(head.radius(6.0).shrink())
        .on_press(move |s: &mut State| {
            if let Some(chat) = s.chat_mut().and_then(ChatPage::existing_mut)
                && !chat.open_groups.remove(&toggle)
            {
                chat.open_groups.insert(toggle.clone());
            }
        });
    let mut group = col()
        .key(format!("tools-{key}"))
        .w_full()
        .child(row().w_full().child(head));
    if open {
        let last = tools.len().saturating_sub(1);
        let rail = tools
            .iter()
            .enumerate()
            .map(|(i, tool)| tool_row(k, tool, i == last));
        group = group.child(col().w_full().ml(RAIL_X).children(rail));
    }
    group
}

/// Where the rail's trunk runs, from the group's left edge.
const RAIL_X: f32 = 8.0;
/// The desktop's activity rows: one line each, branching off the trunk.
const TOOL_ROW_HEIGHT: f32 = 28.0;
const TOOL_HEADER_HEIGHT: f32 = 24.0;
const TOOL_TEXT: f32 = 12.0;

/// One call on the rail, as the desktop draws it: an elbow off the trunk,
/// the kind's icon, its verb, and its subject on one truncated line. File
/// tools show the file's name on a badge instead of its path.
fn tool_row(k: &Kit, tool: &ToolCall, last: bool) -> El {
    let failed = tool.state == ToolState::Failed;
    let tint = if failed {
        tones(k).failed
    } else {
        k.c.text_muted
    };
    let style = TextStyle {
        size: TOOL_TEXT,
        line_height: 18.0,
        ..k.ty.ui
    };
    let elbow = rune::el()
        .w(12.0)
        .h(TOOL_ROW_HEIGHT / 2.0)
        .border_sides([0.0, 0.0, 1.0, 1.0], k.c.border_muted)
        .corners([0.0, 0.0, 0.0, 6.0]);
    // The trunk runs on past every row but the last, which ends in its elbow.
    let mut branch = col().w(12.0).h(TOOL_ROW_HEIGHT).child(elbow);
    if !last {
        branch = branch.border_sides([0.0, 0.0, 0.0, 1.0], k.c.border_muted);
    }
    let lead = match tool.state {
        ToolState::Running => busy(12.0, k),
        _ => icon(icons::tool(tool.kind), 14.0).color(tint),
    };
    let subject = if FILE_KINDS.contains(&tool.kind) && !tool.arg.is_empty() {
        file_badge(k, tool.arg, failed)
    } else {
        text(single_line(&tool_subject(tool)))
            .font(style)
            .color(tint)
            .truncate()
            .min_w(0.0)
    };
    row()
        .w_full()
        .h(TOOL_ROW_HEIGHT)
        .center_y()
        .child(branch)
        .child(rune::el().w(14.0).center().child(lead).ml(4.0).mr(8.0))
        .child(text(tool_verb(tool.kind)).font(style).color(tint).mr(8.0))
        .child(subject.shrink())
}

/// The kinds whose subject is a file.
const FILE_KINDS: [&str; 4] = ["read", "write", "edit", "patch"];

/// The file a call touched, by name, on the desktop's soft badge with the
/// file type's colour icon.
fn file_badge(k: &Kit, path: &str, failed: bool) -> El {
    let name = path
        .rsplit(['/', '\\'])
        .find(|s| !s.is_empty())
        .unwrap_or(path);
    let well = if k.dark { Color::BLACK } else { Color::WHITE }.alpha(0.16);
    row()
        .h(22.0)
        .gap(6.0)
        .pl(1.0)
        .pr(6.0)
        .radius(5.0)
        .center_y()
        .min_w(0.0)
        .fill(k.c.text_base.alpha(0.06))
        .child(
            rune::el()
                .size(20.0, 20.0)
                .radius(4.0)
                .fill(well)
                .center()
                .child(icon(file_icons::file(path), 14.0)),
        )
        .child(
            text(name.to_owned())
                .font(TextStyle {
                    size: TOOL_TEXT,
                    ..k.ty.ui
                })
                .color(if failed {
                    tones(k).failed
                } else {
                    k.c.text_base.alpha(0.85)
                })
                .truncate()
                .min_w(0.0),
        )
}

/// What a call's line names after its verb, as the desktop words it.
fn tool_subject(tool: &ToolCall) -> String {
    match tool.kind {
        "mcp" => tool.arg.replacen('/', " · ", 1),
        _ => tool.arg.to_owned(),
    }
}

/// `text` on one line, its runs of whitespace (newlines too) one space each.
fn single_line(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
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
                        let Some(form) = s
                            .chat_mut()
                            .and_then(ChatPage::existing_mut)
                            .and_then(|c| c.question.as_mut())
                        else {
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
