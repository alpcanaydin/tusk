//! "About Tusk": a small window with the logo and version. A GPUI window
//! (not the native About panel) so Esc and ⌘W close it like Tusk's other
//! secondary windows — GPUI's menu shortcuts never reach a native panel.

use gpui_kit::component::{ActiveTheme as _, Root, TitleBar};
use gpui_kit::*;

use crate::icons::DbIcon;

#[derive(Default)]
struct OpenAbout(Option<AnyWindowHandle>);
impl Global for OpenAbout {}

pub struct AboutWindow {
    focus: FocusHandle,
}

impl AboutWindow {
    /// Open (or focus) the About window.
    pub fn open(cx: &mut App) {
        if let Some(handle) = cx.default_global::<OpenAbout>().0
            && cx
                .update_window(handle, |_, window, _| window.activate_window())
                .is_ok()
        {
            return;
        }
        let bounds = Bounds::centered(None, size(px(300.), px(220.)), cx);
        let result = cx.open_window(
            WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(bounds)),
                is_resizable: false,
                is_minimizable: false,
                focus: !crate::background(),
                ..TitleBar::window_options()
            },
            |window, cx| {
                let view = cx.new(|cx| AboutWindow {
                    focus: cx.focus_handle(),
                });
                view.read(cx).focus.clone().focus(window, cx);
                cx.new(|cx| Root::new(view, window, cx))
            },
        );
        match result {
            Ok(handle) => cx.set_global(OpenAbout(Some(handle.into()))),
            Err(e) => log::warn!("about window failed to open: {e}"),
        }
    }
}

impl Render for AboutWindow {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let t = cx.theme();
        div()
            .track_focus(&self.focus)
            .key_context(crate::dialog_keys::CONTEXT)
            .on_action(crate::dialog_keys::close)
            .on_action(|_: &crate::dialog_keys::DialogConfirm, window, _| window.remove_window())
            .size_full()
            .flex()
            .flex_col()
            .bg(t.background)
            .text_color(t.foreground)
            .font_family(crate::settings::ui_font())
            .child(TitleBar::new())
            .child(
                div()
                    .flex_1()
                    .flex()
                    .flex_col()
                    .items_center()
                    .justify_center()
                    .gap_2()
                    .pb(px(16.))
                    .child(DbIcon::Postgres.icon_px(64.))
                    .child(
                        div()
                            .text_size(px(18.))
                            .font_weight(FontWeight::SEMIBOLD)
                            .child("Tusk"),
                    )
                    .child(
                        div()
                            .text_size(px(12.))
                            .text_color(t.muted_foreground)
                            .child(format!("Version {}", env!("CARGO_PKG_VERSION"))),
                    ),
            )
    }
}
