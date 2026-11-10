//! The phone app's icons (`relay/assets/app.js` `ICONS`), with each group's
//! attributes moved onto its elements.

use std::sync::LazyLock;

use rune::Icon;

const fn icon(name: &'static str, view: f32, body: &'static str) -> Icon {
    Icon { name, view, body }
}

pub static BACK: Icon = icon(
    "back",
    24.0,
    r#"<path d="M14.75 17.5 9.25 12 14.75 6.5" fill="none" stroke="currentColor" stroke-width="1.75" stroke-linecap="round" stroke-linejoin="round"/>"#,
);
pub static COPY: Icon = icon(
    "copy",
    24.0,
    r#"<g fill="none" stroke="currentColor" stroke-width="1.5"><path d="M6.6 11.4c0-2.263 0-3.394.703-4.097C8.006 6.6 9.137 6.6 11.4 6.6h4.8c2.263 0 3.394 0 4.097.703c.703.703.703 1.834.703 4.097v4.8c0 2.263 0 3.394-.703 4.097c-.703.703-1.834.703-4.097.703h-4.8c-2.263 0-3.394 0-4.097-.703C6.6 19.594 6.6 18.463 6.6 16.2z"/><path d="M17.396 6.6c-.005-1.774-.05-2.707-.657-3.314C16.036 2.583 14.905 2.583 12.642 2.583h-3.6c-2.263 0-3.394 0-4.097.703c-.703.703-.703 1.834-.703 4.097v3.6c0 2.263 0 3.394.703 4.097c.433.433.996.587 1.655.635"/></g>"#,
);
pub static CHEV_DOWN: Icon = icon(
    "chev-down",
    24.0,
    r#"<path d="M17.5 9.25 12 14.75 6.5 9.25" fill="none" stroke="currentColor" stroke-width="1.75" stroke-linecap="round" stroke-linejoin="round"/>"#,
);
pub static PLUS: Icon = icon(
    "plus",
    24.0,
    r#"<path d="M12 5v14M5 12h14" fill="none" stroke="currentColor" stroke-width="1.75" stroke-linecap="round" stroke-linejoin="round"/>"#,
);
pub static MORE: Icon = icon(
    "more",
    24.0,
    r#"<circle cx="3.75" cy="12" r="1.6" fill="currentColor"/><circle cx="12" cy="12" r="1.6" fill="currentColor"/><circle cx="20.25" cy="12" r="1.6" fill="currentColor"/>"#,
);
pub static SEND: Icon = icon(
    "send",
    24.0,
    r#"<path d="M12 20V4m0 0l6 6m-6-6l-6 6" fill="none" stroke="currentColor" stroke-linecap="round" stroke-linejoin="round" stroke-width="1.75"/>"#,
);
pub static STOP: Icon = icon(
    "stop",
    10.0,
    r#"<rect x="1" y="1" width="8" height="8" rx="2" fill="currentColor"/>"#,
);
pub static SEARCH: Icon = icon(
    "search",
    24.0,
    r#"<circle cx="11.5" cy="11.5" r="9.5" fill="none" stroke="currentColor" stroke-width="1.5"/><path d="M18.5 18.5L22 22" fill="none" stroke="currentColor" stroke-width="1.5" stroke-linecap="round"/>"#,
);
pub static CHECK: Icon = icon(
    "check",
    16.0,
    r#"<path d="M3.5 8.5l3 3 6-7" fill="none" stroke="currentColor" stroke-width="1.6" stroke-linecap="round" stroke-linejoin="round"/>"#,
);
pub static TERMINAL: Icon = icon(
    "terminal",
    16.0,
    r#"<rect x="1.75" y="2.25" width="12.5" height="11.5" rx="3.25" fill="none" stroke="currentColor" stroke-width="1.25"/><path d="M4.75 6.1 6.65 8l-1.9 1.9" fill="none" stroke="currentColor" stroke-width="1.25" stroke-linecap="round" stroke-linejoin="round"/><path d="M8.4 10.4h2.85" fill="none" stroke="currentColor" stroke-width="1.25" stroke-linecap="round"/>"#,
);
pub static READ: Icon = icon(
    "read",
    24.0,
    r#"<rect x="3" y="2" width="18" height="20" rx="5" fill="none" stroke="currentColor" stroke-width="1.5"/><path d="M8 10h8m-8 4h5" fill="none" stroke="currentColor" stroke-width="1.5" stroke-linecap="round"/>"#,
);
pub static EDIT: Icon = icon(
    "edit",
    24.0,
    r#"<path d="M14.36 4.08l.93-.93a3.93 3.93 0 0 1 5.56 5.56l-.93.93M14.36 4.08l-8.52 8.52c-.58.58-.87.87-1.11 1.19a6.6 6.6 0 0 0-.75 1.21c-.17.36-.3.75-.56 1.53l-1.1 3.28 3.28-1.1c.78-.26 1.17-.39 1.53-.56a6.6 6.6 0 0 0 1.21-.75c.32-.25.61-.53 1.19-1.11l8.52-8.52" fill="none" stroke="currentColor" stroke-width="1.5" stroke-linejoin="round"/>"#,
);
pub static FETCH: Icon = icon(
    "fetch",
    24.0,
    r#"<circle cx="12" cy="12" r="10" fill="none" stroke="currentColor" stroke-width="1.5"/><ellipse cx="12" cy="12" rx="4" ry="10" fill="none" stroke="currentColor" stroke-width="1.5"/><path d="M2 12h20" fill="none" stroke="currentColor" stroke-width="1.5" stroke-linecap="round"/>"#,
);
pub static TODO: Icon = icon(
    "todo",
    24.0,
    r#"<path d="M2 5.5L3.214 7L7.5 3M2 12.5L3.214 14L7.5 10M2 19.5L3.214 21L7.5 17" fill="none" stroke="currentColor" stroke-width="1.5" stroke-linecap="round" stroke-linejoin="round"/><path d="M22 19H12m10-7H12m10-7H12" fill="none" stroke="currentColor" stroke-width="1.5" stroke-linecap="round"/>"#,
);
pub static TOOL: Icon = icon(
    "tool",
    24.0,
    r#"<rect x="2.5" y="2.5" width="8" height="8" rx="2.5" fill="none" stroke="currentColor" stroke-width="1.5"/><rect x="13.5" y="2.5" width="8" height="8" rx="2.5" fill="none" stroke="currentColor" stroke-width="1.5"/><rect x="2.5" y="13.5" width="8" height="8" rx="2.5" fill="none" stroke="currentColor" stroke-width="1.5"/><rect x="13.5" y="13.5" width="8" height="8" rx="2.5" fill="none" stroke="currentColor" stroke-width="1.5"/>"#,
);
pub static BOT: Icon = icon(
    "bot",
    24.0,
    r#"<rect x="4.75" y="7.75" width="14.5" height="11.5" rx="2.75" fill="none" stroke="currentColor" stroke-width="1.5"/><path d="M12 7.5V5.25M9.25 12.25V14M14.75 12.25V14M2 12.5V15M22 12.5V15" fill="none" stroke="currentColor" stroke-width="1.5" stroke-linecap="round"/><circle cx="12" cy="3.75" r="1.25" fill="currentColor"/>"#,
);
pub static ARCHIVE: Icon = icon(
    "archive",
    24.0,
    r#"<path d="M2 12c0-4.714 0-7.071 1.464-8.536C4.93 2 7.286 2 12 2s7.071 0 8.535 1.464C22 4.93 22 7.286 22 12" fill="none" stroke="currentColor" stroke-width="1.5"/><rect x="2" y="6" width="20" height="16" rx="5" fill="none" stroke="currentColor" stroke-width="1.5"/><path d="m9.5 14.4l1.429 1.6l3.571-4" fill="none" stroke="currentColor" stroke-width="1.5" stroke-linecap="round" stroke-linejoin="round"/>"#,
);
pub static RESTART: Icon = icon(
    "restart",
    24.0,
    r#"<path d="M4.27 11a8 8 0 1 1 2.07 4.66" fill="none" stroke="currentColor" stroke-width="1.5" stroke-linecap="round"/><path d="M6.84 20.66l-.63-2.86l2.86-.63" fill="none" stroke="currentColor" stroke-width="1.5" stroke-linecap="round" stroke-linejoin="round"/>"#,
);
pub static SKILL: Icon = icon(
    "skill",
    24.0,
    r#"<path d="M3.84 7.92a2.88 2.88 0 0 1 4.08-4.08l12.23 12.23a2.88 2.88 0 0 1-4.08 4.08Z" fill="none" stroke="currentColor" stroke-width="1.5"/><path d="M6 10L10 6" fill="none" stroke="currentColor" stroke-width="1.5" stroke-linecap="round"/>"#,
);
pub static ALERT: Icon = icon(
    "alert",
    24.0,
    r#"<path d="M5.312 10.762C8.23 5.587 9.689 3 12 3s3.77 2.587 6.688 7.762l.364.644c2.425 4.3 3.638 6.45 2.542 8.022S17.786 21 12.364 21h-.728c-5.422 0-8.134 0-9.23-1.572s.117-3.722 2.542-8.022z" fill="none" stroke="currentColor" stroke-width="1.5"/><path d="M12 8v5" fill="none" stroke="currentColor" stroke-width="1.5" stroke-linecap="round"/><circle cx="12" cy="16" r="1" fill="currentColor"/>"#,
);
pub static BRANCH: Icon = icon(
    "branch",
    24.0,
    r#"<path d="M6 8.5v7M6 12h10a2 2 0 0 0 2-2V8.5" fill="none" stroke="currentColor" stroke-width="1.75" stroke-linecap="round" stroke-linejoin="round"/><circle cx="6" cy="6.25" r="2.25" fill="none" stroke="currentColor" stroke-width="1.75"/><circle cx="6" cy="17.75" r="2.25" fill="none" stroke="currentColor" stroke-width="1.75"/><circle cx="18" cy="6.25" r="2.25" fill="none" stroke="currentColor" stroke-width="1.75"/>"#,
);

static MARK_PI: Icon = icon(
    "mark-pi",
    800.0,
    r#"<path fill="currentColor" fill-rule="evenodd" d="M165.29 165.29H517.36V400H400V517.36H282.65V634.72H165.29ZM282.65 282.65V400H400V282.65ZM517.36 400H634.72V634.72H517.36Z"/>"#,
);
static MARK_CURSOR: Icon = icon(
    "mark-cursor",
    532.0,
    r#"<path fill="currentColor" d="M457.43,125.94L244.42,2.96c-6.84-3.95-15.28-3.95-22.12,0L9.3,125.94c-5.75,3.32-9.3,9.46-9.3,16.11v247.99c0,6.65,3.55,12.79,9.3,16.11l213.01,122.98c6.84,3.95,15.28,3.95,22.12,0l213.01-122.98c5.75-3.32,9.3-9.46,9.3-16.11v-247.99c0-6.65-3.55-12.79-9.3-16.11h-.01ZM444.05,151.99l-205.63,356.16c-1.39,2.4-5.06,1.42-5.06-1.36v-233.21c0-4.66-2.49-8.97-6.53-11.31L24.87,145.67c-2.4-1.39-1.42-5.06,1.36-5.06h411.26c5.84,0,9.49,6.33,6.57,11.39h-.01Z"/>"#,
);
static MARK_OPENCODE: Icon = icon(
    "mark-opencode",
    30.0,
    r#"<path fill="currentColor" fill-rule="evenodd" d="M27 0H3V30H27V0ZM21 6H9V24H21V6Z"/><path fill="currentColor" fill-opacity="0.45" d="M21 12H9V24H21V12Z"/>"#,
);

pub static MIC: Icon = icon(
    "mic",
    24.0,
    r#"<rect x="8.75" y="2.75" width="6.5" height="11.5" rx="3.25" fill="none" stroke="currentColor" stroke-width="1.75"/><path d="M5.5 11a6.5 6.5 0 0 0 13 0M12 17.5v3.75" fill="none" stroke="currentColor" stroke-width="1.75" stroke-linecap="round"/>"#,
);
pub static CLOSE: Icon = icon(
    "close",
    24.0,
    r#"<path d="M17 7 7 17M7 7l10 10" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round"/>"#,
);
/// The track under a context ring.
pub static RING: Icon = icon(
    "ring",
    16.0,
    r#"<circle cx="8" cy="8" r="6" fill="none" stroke="currentColor" stroke-width="2"/>"#,
);

/// The used share of a context ring, one per percent: icons are static SVG.
static ARCS: LazyLock<Vec<Icon>> = LazyLock::new(|| (0..=100).map(arc).collect());

fn arc(percent: u32) -> Icon {
    let turn = f64::from(percent) / 100.0 * std::f64::consts::TAU;
    let (x, y) = (8.0 + 6.0 * turn.sin(), 8.0 - 6.0 * turn.cos());
    let large = u8::from(percent > 50);
    let shape = match percent {
        0 => String::new(),
        100 => {
            r#"<circle cx="8" cy="8" r="6" fill="none" stroke="currentColor" stroke-width="2"/>"#
                .to_owned()
        }
        _ => format!(
            r#"<path d="M8 2A6 6 0 {large} 1 {x:.3} {y:.3}" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round"/>"#
        ),
    };
    // Built once per percent for the life of the app.
    icon(
        Box::leak(format!("ring-{percent}").into_boxed_str()),
        16.0,
        Box::leak(shape.into_boxed_str()),
    )
}

/// The arc of a context ring `fraction` full.
pub fn ring(fraction: f64) -> &'static Icon {
    let percent = (fraction.clamp(0.0, 1.0) * 100.0).round() as usize;
    &ARCS[percent]
}

/// The harness's brand mark: Rune's provider marks where one fits, else a bot.
pub fn mark(harness: &str) -> &'static Icon {
    let provider = match harness {
        "pi" => return &MARK_PI,
        "cursor" => return &MARK_CURSOR,
        "opencode" => return &MARK_OPENCODE,
        "claude-code" => "anthropic",
        "codex" => "openai",
        "grok" => "xai",
        "antigravity" => "google",
        _ => return &BOT,
    };
    rune_theme_opencode::provider_icons::named(provider).unwrap_or(&BOT)
}

/// The icon for a tool call's kind, as the desktop shows it.
pub fn tool(kind: &str) -> &'static Icon {
    match kind {
        "exec" => &TERMINAL,
        "read" => &READ,
        "write" | "edit" | "patch" => &EDIT,
        "search" | "glob" => &SEARCH,
        "web search" | "fetch" => &FETCH,
        "todo" => &TODO,
        _ => &TOOL,
    }
}
