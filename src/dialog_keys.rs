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

/// Confirm a destructive action with a native prompt that Return can't
/// accept: `[Cancel, verb]`. GPUI gives the Cancel button Escape, which
/// replaces the Return the first button would get, so Return does nothing,
/// Esc cancels and only a click (or Space on the focused verb) proceeds —
/// a stray Return, e.g. one aimed at the window behind, never discards,
/// drops or restores. Resolves to `true` only when `verb` was chosen.
pub fn confirm(
    window: &mut Window,
    level: PromptLevel,
    message: &str,
    detail: Option<&str>,
    verb: &str,
    cx: &mut App,
) -> impl std::future::Future<Output = bool> + use<> {
    let answer = window.prompt(
        level,
        message,
        detail,
        &[
            PromptButton::Cancel("Cancel".into()),
            PromptButton::new(verb.to_string()),
        ],
        cx,
    );
    async move { answer.await == Ok(1) }
}
