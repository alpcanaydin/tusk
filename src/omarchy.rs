//! Omarchy theme support: follow the desktop's active theme.
//!
//! Omarchy (https://omarchy.org) writes the active theme's palette to
//! `~/.local/state/omarchy/current/theme/colors.toml` (older releases:
//! `~/.config/omarchy/current/theme/`). `omarchy-theme-set` builds the next
//! theme in a staging directory and moves it into place, so the file is
//! re-read by path (polled), never watched by inode.
//!
//! The palette becomes a [`Pal`] named [`NAME`] and goes through the same
//! `themes::config_json` path as every built-in theme.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Mutex;
use std::time::Duration;

use gpui_kit::{App, AsyncApp};

use crate::themes::Pal;

/// Theme name shown for the Omarchy palette.
pub const NAME: &str = "Omarchy";

/// Candidate theme directories, newest layout first.
fn theme_dirs() -> Vec<PathBuf> {
    let Some(home) = dirs::home_dir() else {
        return Vec::new();
    };
    vec![
        home.join(".local/state/omarchy/current/theme"),
        home.join(".config/omarchy/current/theme"),
    ]
}

/// The active theme's `colors.toml` and whether it is marked light.
fn read_current() -> Option<(String, bool)> {
    theme_dirs().into_iter().find_map(|dir| {
        let text = std::fs::read_to_string(dir.join("colors.toml")).ok()?;
        Some((text, dir.join("light.mode").exists()))
    })
}

/// Is Omarchy's theme present on this machine?
pub fn detected() -> bool {
    theme_dirs().iter().any(|d| d.join("colors.toml").is_file())
}

/// Last parsed palette, keyed by the file contents it came from. Each
/// distinct theme is leaked once (`Pal` holds `&'static` data, like the
/// built-in table); switching back and forth reuses nothing, but a theme
/// switch is a rare, user-driven event.
static CURRENT: Mutex<Option<(String, bool, &'static Pal)>> = Mutex::new(None);

/// The active Omarchy palette, if Omarchy's theme is present and parses.
pub fn palette() -> Option<&'static Pal> {
    let (text, light_marker) = read_current()?;
    let mut cur = CURRENT.lock().ok()?;
    if let Some((t, l, p)) = cur.as_ref()
        && *t == text
        && *l == light_marker
    {
        return Some(p);
    }
    let pal: &'static Pal = Box::leak(Box::new(parse(&text, light_marker)?));
    *cur = Some((text, light_marker, pal));
    Some(pal)
}

/// Poll the theme file and re-theme when it changes while the appearance
/// is "Omarchy". Only started when Omarchy is present at launch.
pub fn watch(cx: &mut App) {
    if !detected() {
        return;
    }
    cx.spawn(async move |cx: &mut AsyncApp| {
        let mut seen = read_current();
        loop {
            cx.background_executor().timer(Duration::from_secs(1)).await;
            let now = read_current();
            if now == seen {
                continue;
            }
            seen = now;
            if crate::settings::get().appearance != crate::settings::Appearance::Omarchy {
                continue;
            }
            cx.update(|cx| {
                crate::theme::apply(cx);
                cx.refresh_windows();
            });
        }
    })
    .detach();
}

/// `key = "#rrggbb"` pairs of a `colors.toml` (the only shape Omarchy
/// writes: flat, one string per line). Keys are lowercased; legacy short
/// names and `colorN` are kept as-is for [`parse`] to fall back on.
fn entries(text: &str) -> HashMap<String, String> {
    text.lines()
        .map(str::trim)
        .filter(|l| !l.starts_with('#') && !l.starts_with('['))
        .filter_map(|line| {
            let (k, v) = line.split_once('=')?;
            let v = v.trim();
            // The quoted value; anything after the closing quote is a comment.
            let v = ['"', '\'']
                .iter()
                .find_map(|q| v.strip_prefix(*q)?.split_once(*q).map(|(s, _)| s))
                .unwrap_or_else(|| v.split_whitespace().next().unwrap_or(""));
            Some((k.trim().to_lowercase(), v.to_string()))
        })
        .collect()
}

fn hex(v: &str) -> Option<u32> {
    let h = v.trim().trim_start_matches('#').trim_start_matches("0x");
    let h = match h.len() {
        6 => h.to_string(),
        // #rgb
        3 => h.chars().flat_map(|c| [c, c]).collect(),
        // #rrggbbaa: drop alpha
        8 => h[..6].to_string(),
        _ => return None,
    };
    u32::from_str_radix(&h, 16).ok()
}

fn mix(a: u32, b: u32, t: f32) -> u32 {
    let ch = |c: u32, s: u32| ((c >> s) & 0xFF) as f32;
    let m = |s: u32| ((ch(a, s) + (ch(b, s) - ch(a, s)) * t).round() as u32) << s;
    m(16) | m(8) | m(0)
}

fn lum(c: u32) -> f32 {
    let ch = |s: u32| ((c >> s) & 0xFF) as f32;
    0.2126 * ch(16) + 0.7152 * ch(8) + 0.0722 * ch(0)
}

/// Build a Tusk palette from `colors.toml`. `None` without a background
/// and a foreground; every other key has a fallback.
fn parse(text: &str, light_marker: bool) -> Option<Pal> {
    let e = entries(text);
    // Canonical name first, then the legacy / ANSI aliases.
    let get = |keys: &[&str]| keys.iter().find_map(|k| e.get(*k).and_then(|v| hex(v)));
    let bg = get(&["background", "bg", "color0"])?;
    let fg = get(&["foreground", "fg", "color7"])?;
    let light = match e.get("mode").map(|m| m.to_lowercase()) {
        Some(m) if m == "light" => true,
        Some(m) if m == "dark" => false,
        _ => light_marker || lum(bg) > lum(fg),
    };
    let dark_bg = get(&["dark_background", "dark_bg"])
        .unwrap_or_else(|| mix(bg, if light { 0xFFFFFF } else { 0x000000 }, 0.25));
    let lighter_bg =
        get(&["lighter_background", "lighter_bg"]).unwrap_or_else(|| mix(bg, fg, 0.08));
    let bright_fg = get(&["bright_foreground", "bright_fg", "color15"]).unwrap_or(fg);
    let dark_fg = get(&["dark_foreground", "dark_fg"]).unwrap_or_else(|| mix(fg, bg, 0.45));
    let muted = get(&["muted", "color8"]).unwrap_or(dark_fg);
    let selection =
        get(&["selection", "selection_background"]).unwrap_or_else(|| mix(bg, fg, 0.16));
    let red = get(&["red", "color1"]).unwrap_or(0xE5484D);
    let green = get(&["green", "color2"]).unwrap_or(0x3FB950);
    let yellow = get(&["yellow", "color3"]).unwrap_or(0xE8C84A);
    let blue = get(&["blue", "color4"]).unwrap_or(0x4A90F0);
    let magenta = get(&["magenta", "color5"]).unwrap_or(blue);
    let cyan = get(&["cyan", "color6"]).unwrap_or(blue);
    let orange = get(&["orange", "bright_yellow", "color11"]).unwrap_or(yellow);
    let accent = get(&["accent"]).unwrap_or(blue);
    // Text on the accent: whichever end of the theme ramp reads better.
    let accent_fg = if (lum(accent) - lum(dark_bg)).abs() >= (lum(accent) - lum(bright_fg)).abs() {
        dark_bg
    } else {
        bright_fg
    };
    Some(Pal {
        name: NAME,
        light,
        bg,
        surface: dark_bg,
        elevated: lighter_bg,
        input: lighter_bg,
        border: mix(bg, muted, 0.55),
        hover: mix(bg, lighter_bg, 0.6),
        selection,
        fg,
        // Between the text and Omarchy's dim foreground: readable, but
        // clearly secondary.
        muted_fg: mix(fg, dark_fg, 0.4),
        accent,
        accent_fg,
        red,
        green,
        yellow,
        blue,
        magenta,
        cyan,
        keyword: magenta,
        function: blue,
        string: green,
        number: orange,
        constant: orange,
        type_: cyan,
        operator: get(&["bright_cyan", "color14"]).unwrap_or(cyan),
        punct: mix(fg, muted, 0.3),
        variable: bright_fg,
        property: get(&["bright_blue", "color12"]).unwrap_or(blue),
        comment: dark_fg,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const TOKYO: &str = r##"mode = "dark"

accent = "#7aa2f7"
selection = "#292e42"
muted = "#414868"

background = "#1a1b26"
dark_background = "#13141c"
darker_background = "#0e0e14"
lighter_background = "#24283b"

foreground = "#a9b1d6"
dark_foreground = "#565f89"
light_foreground = "#b4bee6"
bright_foreground = "#c0caf5"

red = "#f7768e"
yellow = "#e0af68"
green = "#9ece6a"
cyan = "#449dab"
blue = "#7aa2f7"
magenta = "#ad8ee6"
"##;

    #[test]
    fn canonical_keys() {
        let p = parse(TOKYO, false).unwrap();
        assert!(!p.light);
        assert_eq!(p.bg, 0x1A1B26);
        assert_eq!(p.surface, 0x13141C);
        assert_eq!(p.elevated, 0x24283B);
        assert_eq!(p.fg, 0xA9B1D6);
        assert_eq!(p.accent, 0x7AA2F7);
        assert_eq!(p.selection, 0x292E42);
        assert_eq!(p.red, 0xF7768E);
        assert_eq!(p.keyword, 0xAD8EE6);
        assert_eq!(p.string, 0x9ECE6A);
        assert_eq!(p.comment, 0x565F89);
        // No orange in this file: numbers fall back to yellow.
        assert_eq!(p.number, 0xE0AF68);
        // Dark accent text on a light-ish blue.
        assert_eq!(p.accent_fg, 0x13141C);
    }

    #[test]
    fn legacy_keys_and_canonical_wins() {
        let text = r##"
bg = "#101010"
fg = "#e0e0e0"
dark_bg = "#050505"
background = "#202020" # canonical wins
color1 = "#ff0000"
color4 = "#0000ff"
"##;
        let p = parse(text, false).unwrap();
        assert_eq!(p.bg, 0x202020);
        assert_eq!(p.fg, 0xE0E0E0);
        assert_eq!(p.surface, 0x050505);
        assert_eq!(p.red, 0xFF0000);
        // No accent: blue.
        assert_eq!(p.accent, 0x0000FF);
    }

    #[test]
    fn missing_keys_fall_back() {
        assert!(parse("foreground = \"#ffffff\"", false).is_none());
        assert!(parse("", false).is_none());
        let p = parse("background = \"#ffffff\"\nforeground = \"#222222\"", false).unwrap();
        // No mode key: light from the ramp.
        assert!(p.light);
        assert_eq!(p.variable, 0x222222);
        assert_eq!(p.green, 0x3FB950);
    }

    #[test]
    fn mode_marker_and_short_hex() {
        let p = parse("background = \"#000\"\nforeground = \"#fff\"", true).unwrap();
        assert!(p.light);
        assert_eq!(p.bg, 0x000000);
        let p = parse(
            "mode = \"dark\"\nbackground = \"#fff\"\nforeground = \"#000\"",
            true,
        )
        .unwrap();
        assert!(!p.light);
    }

    #[test]
    fn built_theme_config_parses() {
        let p = parse(TOKYO, false).unwrap();
        let cfg = serde_json::from_value::<gpui_kit::component::theme::ThemeConfig>(
            crate::themes::config_json(&p),
        );
        assert!(cfg.is_ok(), "{:?}", cfg.err());
    }
}
