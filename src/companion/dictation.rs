//! macOS dictation, as Edit ▸ Start Dictation runs it. The system listens
//! and types what it hears into the first responder, Rune's view, which
//! passes the words to the focused field.

use objc2::{MainThreadMarker, sel};
use objc2_app_kit::NSApplication;

/// Start dictation into the window's focused field; false when nothing took the action.
pub fn start() -> bool {
    // Rune runs event handlers on the main thread.
    let Some(mtm) = MainThreadMarker::new() else {
        return false;
    };
    let app = NSApplication::sharedApplication(mtm);
    // SAFETY: `startDictation:` is a valid action selector; with no target or
    // sender, AppKit sends it along the responder chain, which NSApplication ends.
    unsafe { app.sendAction_to_from(sel!(startDictation:), None, None) }
}
