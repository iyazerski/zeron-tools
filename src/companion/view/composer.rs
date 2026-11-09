//! The composer under a chat: the text and its images, its `$` skill menu,
//! the model chip with its picker, a new chat's project, the checkout and
//! branch, the context ring, dictation, and Send.

use rune::{El, Image, Key, KeyEvent, SurfaceFit, Tx, auto_grow, col, icon, row, text, text_input};
use rune_kit::{Button, ButtonVariant, Dialog, IconButton, IconButtonVariant, Kit, Spinner};

use super::icons;
use super::widgets::tile;
use crate::companion::app::{Catalog, ChatPage, Config, Pick, Sheet, SkillList, State, Upload};
use crate::companion::data::{Checkout, Skill, levels_of};
use crate::companion::labels::{harness_name, reasoning_name, short_model};
use crate::companion::message::skill_query;

pub fn view(s: &State, page: &ChatPage, k: &Kit, layer: &mut Vec<El>) -> El {
    let working = page.is_working();
    let typed = !page.composer.text.trim().is_empty() || !page.attached.is_empty();
    let stop = !typed && working;
    let field = text_input(&page.composer, k.ty.ui.lh(19.0))
        .key("composer")
        .w_full()
        .color(k.c.text_base)
        .placeholder(
            if page.draft().is_some() {
                "Describe the task"
            } else {
                "Message"
            },
            k.c.text_faint,
        )
        .caret(k.c.text_base, k.selection())
        .on_edit(State::composer_field)
        .on_input_key(composer_key)
        .on_input_paste(State::paste)
        .on_input_change(|s: &mut State, _: &mut Tx| {
            if s.chat().is_some_and(|p| query(p).is_some()) {
                s.load_skills();
            }
        })
        .on_submit(State::send);
    let creating = page.draft().is_some_and(|d| d.creating);
    let ready = page.draft().is_none_or(|d| d.harness.is_some()) && page.uploads_done();
    let send = IconButton::new(if stop { &icons::STOP } else { &icons::SEND })
        .variant(if stop {
            IconButtonVariant::Neutral
        } else {
            IconButtonVariant::Submit
        })
        .label(if stop { "Stop" } else { "Send" })
        .disabled(creating || !(stop || typed && ready))
        .on_press_tx(move |s: &mut State, tx: &mut Tx| if stop { s.stop() } else { s.send(tx) })
        .build(k)
        // Round: a regular icon button is 24 points square.
        .radius(12.0);
    let dictate = IconButton::new(&icons::MIC)
        .variant(IconButtonVariant::GhostMuted)
        .label("Dictate")
        .on_press(State::dictate)
        .build(k)
        .preserve_focus_on_press(true);
    let tools = row()
        .w_full()
        .gap(8.0)
        .center_y()
        .child(rune::spacer())
        .child(model_chip(s, k))
        .child(dictate)
        .child(send);
    let mut field_box = col()
        .key("composer-box")
        .w_full()
        .gap(8.0)
        .px(12.0)
        .pt(12.0)
        .pb(8.0)
        .radius(16.0)
        // A faint tint over the window's own glass: the transcript never scrolls under it.
        .fill(k.c.text_base.alpha(0.035))
        .border(1.0, k.c.border_base);
    if !page.attached.is_empty() {
        field_box = field_box.child(attachments(page, k));
    }
    field_box = field_box.child(auto_grow(field, 1, 8)).child(tools);
    let mut dock = col().key("dock").w_full().px(10.0).pb(6.0).gap(6.0);
    if let Some(menu) = skill_menu(page, k) {
        dock = dock.child(menu);
    }
    dock = dock.child(field_box);
    if typed && working {
        dock = dock.child(
            text("Your message joins the current turn.")
                .font(k.ty.text_xsmall)
                .color(k.c.text_faint)
                .pl(6.0),
        );
    }
    let footer = row()
        .w_full()
        .gap(12.0)
        .px(6.0)
        .center_y()
        .child(meta(page, k))
        .child(rune::spacer())
        .child(context(page, k));
    sheets(s, page, k, layer);
    dock.child(footer)
}

/// The composer's images: a spinner while one saves, red when it didn't.
fn attachments(page: &ChatPage, k: &Kit) -> El {
    row()
        .w_full()
        .gap(8.0)
        .children(page.attached.iter().map(|a| {
            let id = a.id;
            let mut tile = thumb(k, a.thumb.as_ref(), 56.0);
            match &a.upload {
                Upload::Sending => {
                    tile = tile.child(
                        row()
                            .size(56.0, 56.0)
                            .absolute(0.0, 0.0)
                            .center()
                            .child(Spinner::new().size(14.0).build(k)),
                    );
                }
                Upload::Failed(e) => tile = tile.border(1.5, k.c.danger.fg).help(e.clone()),
                Upload::Done(_) => {}
            }
            let remove = k
                .interactive(
                    row()
                        .size(18.0, 18.0)
                        .radius(9.0)
                        .center()
                        .fill(k.c.layer_04)
                        .border(1.0, k.c.border_base)
                        .child(icon(&icons::CLOSE, 10.0).color(k.c.icon_base)),
                )
                .key(format!("detach-{id}"))
                .label("Remove image")
                .preserve_focus_on_press(true)
                .on_press(move |s: &mut State| s.detach(id))
                .absolute(42.0, -4.0);
            tile.key(format!("attached-{id}")).child(remove)
        }))
}

/// An image cropped to a square tile, or an empty tile while it decodes.
pub fn thumb(k: &Kit, image: Option<&Image>, size: f32) -> El {
    let tile = rune::el()
        .size(size, size)
        .radius(8.0)
        .fill(k.c.layer_03)
        .border(1.0, k.c.border_muted);
    // Pixels fill in when they decode, without the view re-running.
    match image.filter(|i| i.error().is_none()) {
        Some(image) => tile.pixels_fit(image.pixels(), SurfaceFit::Fill),
        None => tile,
    }
}

/// The `$` word at the caret, unless Escape closed its menu.
fn query(page: &ChatPage) -> Option<(usize, String)> {
    let edit = &page.composer;
    if !edit.focused || edit.anchor != edit.focus {
        return None;
    }
    skill_query(&edit.text, edit.focus).filter(|(start, _)| page.skills.dismissed != Some(*start))
}

/// The skills that match the `$` word: names that start with it first. `None`
/// hides the menu, as for a `$` word no skill matches (a price).
fn matching<'a>(page: &'a ChatPage, q: &str) -> Option<Result<Vec<&'a Skill>, &'a str>> {
    match &page.skills.list {
        SkillList::Loading => Some(Ok(Vec::new())),
        SkillList::Failed(e) => Some(Err(e)),
        SkillList::Ready(skills) => {
            let mut found: Vec<(bool, &Skill)> = skills
                .iter()
                .filter_map(|s| {
                    let name = s.name.to_lowercase();
                    name.contains(q).then(|| (!name.starts_with(q), s))
                })
                .collect();
            // Stable: names that start with the query first, each in list order.
            found.sort_by_key(|&(later, _)| later);
            let found: Vec<&Skill> = found.into_iter().map(|(_, s)| s).collect();
            (!found.is_empty() || q.is_empty() || skills.is_empty()).then_some(Ok(found))
        }
    }
}

fn composer_key(s: &mut State, key: &KeyEvent, _: &mut Tx) -> bool {
    let Some(page) = s.chat_mut() else {
        return false;
    };
    let Some((start, q)) = query(page) else {
        return false;
    };
    let Some(Ok(found)) = matching(page, &q) else {
        return false;
    };
    let count = found.len();
    let chosen = found
        .get(page.skills.active.min(count.saturating_sub(1)))
        .map(|s| s.name.clone());
    match key.key {
        Key::Escape => page.skills.dismissed = Some(start),
        Key::Down if count > 0 => page.skills.active = (page.skills.active + 1) % count,
        Key::Up if count > 0 => page.skills.active = (page.skills.active + count - 1) % count,
        Key::Tab | Key::Enter if !key.mods.shift => match chosen {
            Some(name) => pick_skill(s, &name),
            None => return false,
        },
        _ => return false,
    }
    true
}

/// Replace the `$` word at the caret with the skill's name.
fn pick_skill(s: &mut State, name: &str) {
    let Some(page) = s.chat_mut() else { return };
    let Some((start, _)) = query(page) else {
        return;
    };
    let edit = &mut page.composer;
    let end = edit.focus;
    let after = &edit.text[end..];
    let spaced = after.starts_with(char::is_whitespace);
    let insert = format!("${name}{}", if spaced { "" } else { " " });
    let text = format!("{}{insert}{after}", &edit.text[..start]);
    edit.replace_text(text);
    edit.set_caret(start + insert.len() + usize::from(spaced));
    page.skills.active = 0;
}

fn skill_menu(page: &ChatPage, k: &Kit) -> Option<El> {
    let (_, q) = query(page)?;
    let found = matching(page, &q)?;
    let note = |s: String| text(s).font(k.ty.small).color(k.c.text_muted).padding(8.0);
    let mut menu = col()
        .key("skills")
        .w_full()
        .max_h(220.0)
        .padding(4.0)
        .radius(12.0)
        .fill(k.c.layer_03)
        .border(1.0, k.c.border_base)
        .scroll(rune::ScrollSpec {
            stick_bottom: false,
        });
    menu = match (&page.skills.list, found) {
        (SkillList::Loading, _) => menu.child(
            row()
                .gap(6.0)
                .padding(8.0)
                .child(Spinner::new().size(11.0).build(k))
                .child(note("Loading skills…".into())),
        ),
        (_, Err(e)) => menu.child(note(format!("Couldn't load the skills: {e}"))),
        (SkillList::Ready(all), Ok(found)) if found.is_empty() => menu.child(note(
            if all.is_empty() {
                "No skills available"
            } else {
                "No matching skills"
            }
            .into(),
        )),
        (_, Ok(found)) => {
            let active = page.skills.active.min(found.len() - 1);
            menu.children(found.into_iter().enumerate().map(|(i, skill)| {
                let name = skill.name.clone();
                let item = row()
                    .w_full()
                    .gap(8.0)
                    .px(8.0)
                    .py(6.0)
                    .radius(8.0)
                    .center_y()
                    .fill(if i == active {
                        k.c.layer_04
                    } else {
                        rune::Color::TRANSPARENT
                    })
                    .child(icon(&icons::SKILL, 13.0).color(k.c.icon_muted))
                    .child(
                        text(skill.name.clone())
                            .font(k.ty.ui_medium)
                            .color(k.c.text_base),
                    )
                    .child(
                        text(skill.description.clone())
                            .font(k.ty.small)
                            .color(k.c.text_faint)
                            .truncate()
                            .grow(1.0)
                            .min_w(0.0),
                    );
                k.interactive(item)
                    .key(format!("skill-{}", skill.name))
                    .preserve_focus_on_press(true)
                    .on_press(move |s: &mut State| pick_skill(s, &name))
            }))
        }
    };
    Some(menu)
}

fn model_chip(s: &State, k: &Kit) -> El {
    let Config {
        harness,
        model,
        reasoning,
    } = s.config();
    let Some(harness) = harness else {
        return rune::el();
    };
    let catalog = s
        .catalogs
        .get(harness)
        .and_then(Catalog::ready)
        .map(|(models, _)| models);
    let label = model.map_or("Default".to_owned(), |m| {
        catalog
            .and_then(|ms| ms.iter().find(|x| x.id == m))
            .and_then(|x| x.label.clone())
            .unwrap_or_else(|| short_model(m).to_owned())
    });
    let mut chip = row()
        .gap(5.0)
        .px(6.0)
        .py(3.0)
        .radius(7.0)
        .center_y()
        .min_w(0.0)
        .child(icon(icons::mark(harness), 13.0).color(k.c.icon_muted))
        .child(
            text(label)
                .font(k.ty.small_medium)
                .color(k.c.text_muted)
                .truncate(),
        );
    if let Some(r) = reasoning {
        chip = chip.child(
            text(reasoning_name(r))
                .font(k.ty.small)
                .color(k.c.text_faint),
        );
    }
    k.interactive(chip)
        .key("model-chip")
        .label("Model")
        .on_press(|s: &mut State| {
            open_sheet(s, Sheet::Model);
            // Retry a catalog that failed to load.
            if let Some(harness) = s.config().harness.map(str::to_owned) {
                s.catalog(&harness);
            }
        })
}

fn open_sheet(s: &mut State, sheet: Sheet) {
    if let Some(page) = s.chat_mut() {
        page.sheet = Some(sheet);
    }
}

fn meta(page: &ChatPage, k: &Kit) -> El {
    let mut out = row().gap(8.0).center_y().min_w(0.0).shrink();
    if let Some(d) = page.draft() {
        let project = d
            .project
            .as_ref()
            .and_then(|id| d.projects.iter().find(|p| &p.id == id));
        let chip = row()
            .gap(5.0)
            .px(6.0)
            .py(3.0)
            .radius(7.0)
            .center_y()
            .child(tile(k, project, 13.0))
            .child(
                text(project.map_or("No project", |p| p.name.as_str()))
                    .font(k.ty.small)
                    .color(k.c.text_muted)
                    .truncate(),
            );
        out = out.child(
            k.interactive(chip)
                .key("project-chip")
                .label("Project")
                .on_press(|s: &mut State| open_sheet(s, Sheet::Project)),
        );
    }
    let Some(info) = &page.info else { return out };
    // Like the desktop footer: checkout and branch only show in a git project.
    let Some(checkout) = info.checkout else {
        return out;
    };
    let faint = |glyph: &'static rune::Icon, label: String| {
        row()
            .gap(5.0)
            .center_y()
            .min_w(0.0)
            .child(icon(glyph, 12.0).color(k.c.icon_faint))
            .child(
                text(label)
                    .font(k.ty.small)
                    .color(k.c.text_faint)
                    .truncate(),
            )
    };
    let checkout = match checkout {
        Checkout::Worktree => "Worktree",
        Checkout::Local => "Local checkout",
    };
    out.child(faint(&icons::FOLDER, checkout.to_owned()))
        .child(faint(
            &icons::BRANCH,
            info.branch.clone().unwrap_or_else(|| "No ref".into()),
        ))
}

/// How full the context window is, as a ring toned as the desktop tones it.
fn context(page: &ChatPage, k: &Kit) -> El {
    let Some(usage) = page.info.as_ref().and_then(|i| i.context_usage) else {
        return rune::el();
    };
    let Some(window) = usage.window.filter(|w| *w > 0) else {
        return rune::el();
    };
    let Some(tokens) = usage.tokens else {
        return text("—").font(k.ty.small).color(k.c.text_faint);
    };
    let fraction = tokens as f64 / window as f64;
    let (arc, label) = if fraction >= 0.9 {
        (k.c.danger.fg, k.c.danger.fg)
    } else if fraction >= 0.75 {
        (k.c.warning.fg, k.c.warning.fg)
    } else {
        (k.c.text_accent, k.c.text_muted)
    };
    let ring = rune::el()
        .size(14.0, 14.0)
        .child(
            icon(&icons::RING, 14.0)
                .absolute(0.0, 0.0)
                .color(k.c.icon_faint)
                .opacity(0.35),
        )
        .child(
            icon(icons::ring(fraction), 14.0)
                .absolute(0.0, 0.0)
                .color(arc),
        );
    row()
        .gap(6.0)
        .center_y()
        .child(ring)
        .child(
            text(format!("{}%", (fraction * 100.0).round()))
                .font(k.ty.small)
                .color(label),
        )
        .help(format!(
            "{tokens} / {window} tokens. {} tokens remaining.",
            window.saturating_sub(tokens)
        ))
}

fn sheets(s: &State, page: &ChatPage, k: &Kit, layer: &mut Vec<El>) {
    let close = |s: &mut State| {
        if let Some(p) = s.chat_mut() {
            p.sheet = None;
        }
    };
    match page.sheet {
        Some(Sheet::Model) => {
            let title = s.config().harness.map_or("Model", harness_name).to_owned();
            Dialog::new("model-sheet", title)
                .width(360.0)
                .open(true)
                .on_close(close)
                .child(model_body(s, page, k))
                .build(k, layer);
        }
        Some(Sheet::Project) => {
            Dialog::new("project-sheet", "Project")
                .width(360.0)
                .open(true)
                .on_close(close)
                .child(project_body(page, k))
                .build(k, layer);
        }
        None => {}
    }
}

fn group_label(k: &Kit, s: &str) -> El {
    text(s.to_owned())
        .font(k.ty.text_xsmall_medium)
        .color(k.c.text_faint)
        .pt(6.0)
}

fn pill(k: &Kit, label: String, on: bool, f: impl Fn(&mut State) + 'static) -> El {
    Button::new(label)
        .variant(if on {
            ButtonVariant::Contrast
        } else {
            ButtonVariant::Outline
        })
        .on_press(move |s: &mut State| {
            if !on {
                f(s)
            }
        })
        .build(k)
}

fn list_row(
    k: &Kit,
    lead: Option<El>,
    label: String,
    on: bool,
    key: String,
    f: impl Fn(&mut State) + 'static,
) -> El {
    let mut item = row()
        .w_full()
        .gap(8.0)
        .px(8.0)
        .py(7.0)
        .radius(8.0)
        .center_y();
    if let Some(lead) = lead {
        item = item.child(lead);
    }
    item = item
        .child(
            text(label)
                .font(k.ty.ui)
                .color(k.c.text_base)
                .truncate()
                .grow(1.0)
                .min_w(0.0),
        )
        .child(if on {
            icon(&icons::CHECK, 13.0).color(k.c.text_base)
        } else {
            rune::el().size(13.0, 13.0)
        });
    k.interactive(item).key(key).on_press(f)
}

fn sheet_scroll(body: El) -> El {
    col()
        .key("sheet-scroll")
        .w_full()
        .grow(1.0)
        .min_h(0.0)
        .px(16.0)
        .pb(16.0)
        .scroll(rune::ScrollSpec {
            stick_bottom: false,
        })
        .child(body)
}

/// Reasoning first, as pills: a harness like Pi lists dozens of models.
fn model_body(s: &State, page: &ChatPage, k: &Kit) -> El {
    let Config {
        harness,
        model: current,
        reasoning,
    } = s.config();
    let (models, harness_levels) = match harness.and_then(|h| s.catalogs.get(h)) {
        Some(Catalog::Ready { models, levels }) => (models, levels),
        Some(Catalog::Failed(e)) => {
            return text(format!("Couldn't load the models: {e}"))
                .font(k.ty.small)
                .color(k.c.text_muted)
                .wrap()
                .padding(16.0);
        }
        Some(Catalog::Loading) | None => {
            return row()
                .w_full()
                .py(24.0)
                .justify_center()
                .child(Spinner::new().build(k));
        }
    };
    let mut body = col().w_full().gap(6.0);
    if let Some(d) = page.draft().filter(|d| d.harnesses.len() > 1) {
        body = body.child(group_label(k, "Agent")).child(
            row()
                .w_full()
                .gap(6.0)
                .flex_wrap()
                .children(d.harnesses.iter().map(|h| {
                    let id = h.clone();
                    pill(
                        k,
                        harness_name(h).to_owned(),
                        d.harness.as_ref() == Some(h),
                        move |s| s.pick(Pick::Harness(id.clone())),
                    )
                })),
        );
    }
    let levels = levels_of(models, harness_levels, current.unwrap_or_default());
    if !levels.is_empty() {
        body = body.child(group_label(k, "Reasoning")).child(
            row()
                .w_full()
                .gap(6.0)
                .flex_wrap()
                .children(levels.iter().map(|l| {
                    let level = l.clone();
                    pill(
                        k,
                        reasoning_name(l),
                        reasoning == Some(l.as_str()),
                        move |s| s.pick(Pick::Reasoning(level.clone())),
                    )
                })),
        );
    }
    if page.is_working() {
        body = body.child(
            text("Changes apply from the next turn.")
                .font(k.ty.small)
                .color(k.c.text_faint),
        );
    }
    body = body
        .child(group_label(k, "Model"))
        .children(models.iter().map(|m| {
            let id = m.id.clone();
            let on = current == Some(m.id.as_str());
            list_row(
                k,
                None,
                m.label.clone().unwrap_or_else(|| m.id.clone()),
                on,
                format!("model-{}", m.id),
                move |s| {
                    if !on {
                        s.pick(Pick::Model(id.clone()));
                    }
                },
            )
        }));
    sheet_scroll(body)
}

fn project_body(page: &ChatPage, k: &Kit) -> El {
    let Some(d) = page.draft() else {
        return rune::el();
    };
    let pick = |project: Option<String>| {
        move |s: &mut State| {
            s.pick_project(project.clone());
            if let Some(p) = s.chat_mut() {
                p.sheet = None;
            }
        }
    };
    let mut body = col().w_full().gap(2.0).child(list_row(
        k,
        Some(tile(k, None, 16.0)),
        "No project".into(),
        d.project.is_none(),
        "project-none".into(),
        pick(None),
    ));
    body = body.children(d.projects.iter().map(|p| {
        let on = d.project.as_ref() == Some(&p.id);
        list_row(
            k,
            Some(tile(k, Some(p), 16.0)),
            p.name.clone(),
            on,
            format!("project-{}", p.id),
            pick(Some(p.id.clone())),
        )
    }));
    sheet_scroll(body)
}
