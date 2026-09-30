//! Keyboard for Tusk's secondary windows (New Connection, Settings, Export,
//! Backup / Restore): Esc and ⌘W close the window, Return presses its
//! default button — the way macOS dialogs behave.
//!
//! A window puts `key_context(CONTEXT)` on its root element (with a tracked
//! focus handle, so the context is on the dispatch path) and handles
//! [`DialogConfirm`]; [`DialogCancel`] closes it unless the view overrides it.
//! Single-line inputs propagate Enter / Escape, so both keys also work while
//! typing; selects, popovers and multi-line editors keep theirs.

use gpui_kit::*;

gpui_kit::actions!(tusk_dialog, [DialogCancel, DialogConfirm]);

pub const CONTEXT: &str = "TuskDialog";

pub fn bind_keys(cx: &mut App) {
    cx.bind_keys([
        KeyBinding::new("escape", DialogCancel, Some(CONTEXT)),
        KeyBinding::new("secondary-w", DialogCancel, Some(CONTEXT)),
        KeyBinding::new("enter", DialogConfirm, Some(CONTEXT)),
    ]);
}

/// Close the window the action came from.
pub fn close(_: &DialogCancel, window: &mut Window, _: &mut App) {
    window.remove_window();
}
