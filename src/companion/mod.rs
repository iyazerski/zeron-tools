//! `companion`: a small floating window with the phone app's chats for the
//! Zeron running on this Mac. A global shortcut shows and hides it; it talks
//! to Zeron's engine through the same tools the phone uses, so it always
//! shows what Zeron shows.

mod api;
mod app;
mod data;
mod labels;
mod message;
mod prefs;
mod view;

use std::path::Path;
use std::sync::Arc;

use anyhow::{Context, Result};
use rune::{App, AppSpec, MacWindowOptions, RunOptions, WindowOptions, WindowSpec};

use crate::service::{COMPANION, RunMode};
use crate::zeron::Zeron;

use self::api::Api;
use self::app::State;
use self::prefs::Prefs;
use self::view::{CORNER, on_key, view};

/// NSWindow collection behavior: on every Space, and over full-screen apps.
const ALL_SPACES: usize = 1 << 0;
const FULL_SCREEN_AUXILIARY: usize = 1 << 8;
/// NSFloatingWindowLevel: above normal windows, under menus.
const FLOATING: isize = 3;
const SIZE: (f32, f32) = (420.0, 620.0);

/// The first spot: the primary display's top right, under the menu bar. It's an
/// initial position, not a placement, so later opens keep where the user dragged it.
fn top_right(displays: &[rune::Display]) -> Option<(i32, i32)> {
    let display = displays.iter().find(|d| d.primary).or(displays.first())?;
    let px = |points: f32| (f64::from(points) * display.scale).round() as i32;
    let right = display.position.0 + i32::try_from(display.size.0).ok()?;
    Some((right - px(SIZE.0 + 24.0), display.position.1 + px(56.0)))
}

/// What `companion` runs with.
pub struct Opts<'a> {
    pub data_dir: &'a Path,
    pub ipc_port: u16,
    /// The global shortcut that shows and hides the window.
    pub shortcut: &'a str,
    /// Show the window at launch instead of waiting for the shortcut.
    pub show: bool,
    pub mode: RunMode,
}

/// Run the window until it quits. It owns the main thread, as macOS needs;
/// Zeron's calls run on a small tokio runtime beside it.
pub fn run(opts: Opts) -> Result<()> {
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .context("starting the runtime")?;
    // Two would both take the shortcut, and each press would toggle twice.
    let _only = rt.block_on(COMPANION.single_instance(opts.data_dir, opts.mode))?;
    let zeron = Arc::new(Zeron::new(opts.ipc_port, opts.data_dir));
    let prefs = Prefs::load(opts.data_dir.join("zeron-tools-companion.json"));
    let handle = rt.handle().clone();

    let spec = AppSpec {
        min_size: Some((340.0, 420.0)),
        fonts: rune_theme_opencode::FONTS,
        families: rune_theme_opencode::FAMILIES,
        on_key: Some(on_key),
        setup: Some(|s, waker| s.images = Some(rune::ImageCache::new(waker))),
        ..AppSpec::new("Zeron Companion", SIZE, || unreachable!(), view)
    };
    let app = App::new(spec).windowless();
    let me = app.handle();
    if opts.show {
        me.update(|s, _| s.toggle_window());
    }
    let app = app
        .init(move || State::new(Api::start(handle, zeron, me), prefs))
        .global_shortcut(opts.shortcut, |s, _| s.toggle_window());
    eprintln!("companion is running; {} shows it", opts.shortcut);
    rune::run_windows_with_args(RunOptions { accessory: true }, [], move |displays| {
        let options = WindowOptions {
            transparent: true,
            borderless: true,
            always_on_top: true,
            resizable: true,
            visible: false,
            position: top_right(displays),
            min_size: Some((340.0, 420.0)),
            ..Default::default()
        }
        .macos(
            // Not a nonactivating panel: Rune moves a panel's content into an NSPanel that
            // `.window_drag()` regions can't move.
            MacWindowOptions::default()
                .level(FLOATING)
                .collection_behavior(ALL_SPACES | FULL_SCREEN_AUXILIARY)
                .corner_radius(CORNER),
        );
        vec![WindowSpec::from_app("companion", app).configured_options(options)]
    });
    drop(rt);
    Ok(())
}
