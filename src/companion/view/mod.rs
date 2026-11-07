//! What the window draws, and its keys: the chat list or a chat's page,
//! under the toast.

mod chat;
mod composer;
mod icons;
mod list;
mod widgets;

use rune::{Cx, El, Key, KeyEvent, SystemMaterial, Tx, col};
use rune_kit::{Kit, Toast, ToastRegion};

use super::app::{Page, State};

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
        if t.error {
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

pub fn on_key(s: &mut State, key: &KeyEvent, tx: &mut Tx) -> bool {
    if key.is_command('w') {
        s.api.app().hide();
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
        Some(page) if page.sheet.is_some() => page.sheet = None,
        Some(page) if page.menu => page.menu = false,
        _ => s.api.app().hide(),
    }
    true
}
