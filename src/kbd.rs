//! Shortcut key caps (⌘⇧O) wherever a shortcut is shown, in the system
//! font: it has the modifier glyphs whatever the UI font is.

use gpui_kit::component::kbd::Kbd;
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;

fn parse_key(key: &str) -> Option<Keystroke> {
    let portable = key
        .strip_prefix("cmd-")
        .map(|rest| format!("secondary-{rest}"));
    Keystroke::parse(portable.as_deref().unwrap_or(key)).ok()
}

/// One shortcut in keymap syntax (`cmd-shift-o`, `cmd-enter`, `escape`)
/// as a row of caps. Chords are space separated.
pub fn caps(keys: &str) -> AnyElement {
    caps_styled(keys, None, px(11.))
}

/// [`caps`] at a given text size.
pub fn caps_sized(keys: &str, size: Pixels) -> AnyElement {
    caps_styled(keys, None, size)
}

/// Caps drawn in `color` (label + a faint border of it), for caps inside a
/// sentence of that color.
fn caps_styled(keys: &str, color: Option<Hsla>, size: Pixels) -> AnyElement {
    let caps = keys.split_whitespace().filter_map(parse_key).map(|k| {
        let kbd = Kbd::new(k)
            .outline()
            .font_family(".SystemUIFont")
            .text_size(size);
        match color {
            Some(c) => kbd
                .text_color(c)
                .border_color(c.opacity(0.4))
                .bg(gpui_kit::transparent_black()),
            None => kbd,
        }
    });
    div()
        .flex()
        .flex_none()
        .gap_1()
        .children(caps)
        .into_any_element()
}

/// Text with `[keys]` segments as caps:
/// `"[cmd-enter] run current · [cmd-shift-enter] run all"`.
/// The caps take the sentence's own `color` (label and border).
pub fn rich_colored(text: &str, color: Hsla) -> Div {
    rich_in(text, Some(color)).text_color(color)
}

fn rich_in(text: &str, color: Option<Hsla>) -> Div {
    let mut row = div()
        .flex()
        .items_center()
        .flex_wrap()
        .gap_x_1p5()
        .gap_y_1();
    let mut rest = text;
    while let Some(open) = rest.find('[') {
        let Some(close) = rest[open..].find(']').map(|c| open + c) else {
            break;
        };
        let before = rest[..open].trim();
        row = row.when(!before.is_empty(), |r| r.child(before.to_string()));
        row = row.child(caps_styled(&rest[open + 1..close], color, px(11.)));
        rest = &rest[close + 1..];
    }
    let tail = rest.trim();
    row.when(!tail.is_empty(), |r| r.child(tail.to_string()))
}

/// For tooltips: `Tooltip::new(..).key_binding(kbd::tip("cmd-t"))`.
pub fn tip(keys: &str) -> Option<Kbd> {
    parse_key(keys).map(Kbd::new)
}

#[cfg(test)]
mod tests {
    #[test]
    fn primary_shortcut_matches_platform() {
        let key = super::parse_key("cmd-s").unwrap();
        assert_eq!(key.modifiers.control, cfg!(not(target_os = "macos")));
        assert_eq!(key.modifiers.platform, cfg!(target_os = "macos"));
    }
}
