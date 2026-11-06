//! Small pieces the list and the chat share: tones, tiles, dots, notes.

use rune::{Color, El, TextStyle, el, icon, row, text};
use rune_kit::{Button, ButtonVariant, IconButton, IconButtonVariant, Kit};
use zeron_relay_proto::Engine;

use super::app::State;
use super::data::{Project, tone_index};
use super::icons;

/// The desktop's project tones (`project_icon.rs`): slate, blue, violet,
/// rose, amber, emerald, teal, orange.
const TONES_DARK: [&str; 8] = [
    "#94a3b8", "#93c5fd", "#c4b5fd", "#fda4af", "#fcd34d", "#6ee7b7", "#5eead4", "#fdba74",
];
const TONES_LIGHT: [&str; 8] = [
    "#475569", "#2563eb", "#7c3aed", "#be123c", "#a16207", "#047857", "#0f766e", "#c2410c",
];

/// Hides the window; the shortcut brings it back.
pub fn hide_button(k: &Kit) -> El {
    IconButton::new(&super::icons::CLOSE)
        .variant(IconButtonVariant::GhostMuted)
        .label("Hide")
        .help("Hide (Esc)")
        .on_press(|s: &mut State| s.api.app().hide())
        .build(k)
}

/// An sRGB `#rrggbb` as Rune's linear colour.
fn hex(s: &str) -> Color {
    let channel = |i: usize| {
        let v = f32::from(u8::from_str_radix(&s[i..i + 2], 16).unwrap_or(0)) / 255.0;
        if v <= 0.04045 {
            v / 12.92
        } else {
            ((v + 0.055) / 1.055).powf(2.4)
        }
    };
    Color {
        r: channel(1),
        g: channel(3),
        b: channel(5),
        a: 1.0,
    }
}

/// The status tones the phone app uses: working, input, failed, done.
pub struct Tones {
    pub working: Color,
    pub input: Color,
    pub failed: Color,
    pub done: Color,
}

pub fn tones(k: &Kit) -> Tones {
    Tones {
        working: k.c.text_muted,
        input: k.c.warning.icon,
        failed: k.c.danger.icon,
        done: k.c.success.icon,
    }
}

/// The project monogram tile; chats without a project read "H" (Home).
pub fn tile(k: &Kit, project: Option<&Project>, size: f32) -> El {
    let index = project.map_or(6, |p| {
        tone_index(if p.path.is_empty() { &p.name } else { &p.path })
    });
    let tone = hex(if k.dark { TONES_DARK } else { TONES_LIGHT }[index]);
    let name = project.map_or("Home", |p| p.name.trim());
    let letter = name
        .chars()
        .next()
        .map_or("?".to_owned(), |c| c.to_uppercase().collect());
    el().size(size, size)
        .radius(size * 0.28)
        .fill(tone.alpha(0.18))
        .center()
        .child(
            text(letter)
                .font(TextStyle::new(size * 0.62, 600.0))
                .color(tone),
        )
}

pub fn dot(color: Color) -> El {
    el().size(7.0, 7.0).radius(3.5).fill(color)
}

/// A faint one-line label.
pub fn faint(k: &Kit, s: impl Into<String>) -> El {
    text(s)
        .font(k.ty.text_small)
        .color(k.c.text_muted)
        .truncate()
}

/// Why Zeron can't answer, with the way out, or nothing while it runs.
pub fn engine_note(s: &State, k: &Kit) -> Option<El> {
    let (title, action) = match &s.engine {
        Engine::Running { .. } => return None,
        Engine::Closed => ("Zeron is closed", true),
        Engine::Outdated { .. } => ("Update zeron-tools to talk to this Zeron", false),
    };
    let mut note = row()
        .w_full()
        .gap(8.0)
        .center_y()
        .padding(10.0)
        .radius(10.0)
        .fill(k.c.warning.bg)
        .child(icon(&icons::ALERT, 14.0).color(k.c.warning.icon))
        .child(
            text(title)
                .font(k.ty.ui)
                .color(k.c.text_base)
                .grow(1.0)
                .truncate(),
        );
    if action {
        note = note.child(
            Button::new("Open")
                .variant(ButtonVariant::Neutral)
                .on_press(|s: &mut State| s.open_zeron())
                .build(k),
        );
    }
    Some(note)
}
