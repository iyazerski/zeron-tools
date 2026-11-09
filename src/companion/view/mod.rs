//! What the window draws, and its keys: the chat list or a chat's page,
//! under the toast.

mod chat;
mod code;
mod composer;
mod copy;
mod icons;
mod list;
mod widgets;

use rune::{ClipboardContent, Cx, El, Key, KeyEvent, SystemMaterial, Tx, col};
use rune_kit::{Kit, Toast, ToastRegion};

use super::app::{Overlay, Page, State, ToastKind};
use super::data::{Message, Part};

/// The window's corner radius.
pub const CORNER: f32 = 14.0;

pub fn view(s: &State, cx: &mut Cx<State>) -> El {
    let k = rune_theme_opencode::kit(cx.system_dark());
    let mut layer = Vec::new();
    let page = match &s.page {
        Page::List => list::view(s, k),
        Page::Chat(page) => chat::view(s, page, k, &mut layer),
    };
    if let Some(t) = &s.toast {
        let mut toast = Toast::new("toast", t.title.clone());
        if !t.body.is_empty() {
            toast = toast.description(t.body.clone());
        }
        if t.kind == ToastKind::Failure {
            toast = toast.icon(k, &icons::ALERT);
        }
        layer.push(
            ToastRegion::new()
                .toast(
                    toast
                        .on_close(|s: &mut State| s.toast = None)
                        .closable()
                        .build(k),
                )
                .build(k),
        );
    }
    let body = col()
        .key("window")
        .w_full()
        .h_full()
        .radius(CORNER)
        .system_material(SystemMaterial::Popover)
        .fill(surface(k))
        .border(1.0, k.c.border_base)
        .child(page);
    rune_kit::root(body, layer)
}

/// The window's tint over the blur: slightly see-through.
fn surface(k: &Kit) -> rune::Color {
    k.c.bg_base.alpha(0.92)
}

/// Copy the transcript's selection with its formatting; false when it has
/// none to keep, and Rune copies the plain text.
fn copy_rich(s: &State, tx: &mut Tx) -> bool {
    let Some(sel) = tx.text_selection().cloned() else {
        return false;
    };
    let Some(read) = s
        .chat()
        .and_then(|p| p.existing())
        .and_then(|c| c.read.as_ref())
    else {
        return false;
    };
    let Some(html) = copy::html(&sel, |key| part_text(&read.messages, key)) else {
        return false;
    };
    tx.copy_rich(ClipboardContent::new(sel.text()).with_html(html));
    true
}

/// The Markdown of the transcript part keyed `{message id}#{index}`, as `chat` keys it.
fn part_text<'a>(messages: &'a [Message], key: &str) -> Option<&'a str> {
    let (id, index) = key.rsplit_once('#')?;
    let message = messages.iter().find(|m| m.id == id)?;
    match message.parts.get(index.parse::<usize>().ok()?)? {
        Part::Text { text } => Some(text),
        _ => None,
    }
}

pub fn on_key(s: &mut State, key: &KeyEvent, tx: &mut Tx) -> bool {
    if key.is_command('w') {
        s.hide();
        return true;
    }
    if key.is_command('c') && copy_rich(s, tx) {
        return true;
    }
    if key.is_command('q') {
        tx.quit();
        return true;
    }
    if key.is_command('n') {
        s.new_chat();
        return true;
    }
    if key.is_command('[') && s.chat().is_some() {
        s.back();
        return true;
    }
    if key.key != Key::Escape {
        return false;
    }
    match s.chat_mut() {
        Some(page) if page.overlay != Overlay::Closed => page.overlay = Overlay::Closed,
        _ => s.hide(),
    }
    true
}
