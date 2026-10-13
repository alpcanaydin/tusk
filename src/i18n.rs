//! Localization on rust-i18n. The locale files in `locales/` are embedded at
//! build time (the `i18n!` call lives in `main.rs`, where the `t!` macro
//! resolves its crate-root helpers); "en" is the default and the fallback.
//!
//! Translate in `render` / at display time, never store translated strings in
//! view structs — a language switch only re-renders windows and rebuilds the
//! menu bar, cached text would freeze in the old language.

use gpui_kit::*;

/// `t!` as an owned `String` for GPUI elements: `tr!("menu.file.new", name = x)`.
#[macro_export]
macro_rules! tr {
    ($($t:tt)*) => {
        rust_i18n::t!($($t)*).into_owned()
    };
}

pub const LANG_SYSTEM: &str = "system";
pub const LANG_EN: &str = "en";
pub const LANG_ZH_CN: &str = "zh-CN";
/// Accepted values of `Prefs.language`.
pub const LANGUAGES: [&str; 3] = [LANG_SYSTEM, LANG_EN, LANG_ZH_CN];

/// The language in effect (the setting resolved against the OS locale).
/// Marks "switch happened" for GPUI code; rust-i18n keeps its own copy.
pub struct ActiveLocale(pub SharedString);
impl Global for ActiveLocale {}

/// The language currently applied.
#[allow(dead_code)] // consumed by views once string extraction lands (batches 1+)
pub fn current(cx: &App) -> SharedString {
    cx.try_global::<ActiveLocale>()
        .map(|l| l.0.clone())
        .unwrap_or_else(|| LANG_EN.into())
}

/// Resolve a `Prefs.language` value to a locale file ("en" / "zh-CN").
fn resolve(setting: &str) -> &'static str {
    match setting {
        LANG_EN => LANG_EN,
        LANG_ZH_CN => LANG_ZH_CN,
        // "system": zh* desktop locales get Simplified Chinese, else English.
        _ => match sys_locale::get_locale() {
            Some(l) if l.starts_with("zh") => LANG_ZH_CN,
            _ => LANG_EN,
        },
    }
}

/// Push the effective language into rust-i18n and the GPUI global.
/// Called at startup and after every language change.
pub fn apply(cx: &mut App) {
    let lang = resolve(&crate::settings::get().language);
    rust_i18n::set_locale(lang);
    cx.set_global(ActiveLocale(lang.into()));
    log::info!("locale: {lang}");
}

/// Change the language setting: persist, switch, rebuild the menu bar
/// (`set_menus` is not re-rendered with the windows), redraw every window.
pub fn set_language(cx: &mut App, value: &str) {
    crate::settings::update(cx, |p| p.language = value.to_string());
    apply(cx);
    crate::menus::refresh(cx);
    sync_placeholders(cx);
    cx.refresh_windows();
}

/// Placeholders of the inputs built once at startup live outside the render
/// pass, so `refresh_windows` cannot re-translate them; update them through
/// the main window directly. No-ops while it isn't open (welcome state,
/// startup, tests).
fn sync_placeholders(cx: &mut App) {
    let (Some(main), Some(app)) = (
        crate::main_window(cx),
        cx.try_global::<crate::app::TuskHandle>()
            .map(|h| h.0.clone()),
    ) else {
        return;
    };
    let _ = cx.update_window(main, |_, window, cx| {
        app.update(cx, |app, cx| app.sync_localized_placeholders(window, cx));
    });
}

#[cfg(test)]
mod tests {
    // Explicit imports: a glob from the crate root also pulls the rust-i18n
    // generated macros, which breaks `#[test]` expansion.
    use super::{LANG_EN, LANG_SYSTEM, LANG_ZH_CN, resolve};
    use std::collections::{BTreeMap, BTreeSet};

    #[test]
    fn explicit_settings_locales_win_over_the_system() {
        assert_eq!(resolve(LANG_EN), LANG_EN);
        assert_eq!(resolve(LANG_ZH_CN), LANG_ZH_CN);
        // "system" and anything unknown fall through to the OS lookup,
        // which always yields one of the two embedded locales.
        assert!(matches!(resolve(LANG_SYSTEM), "en" | "zh-CN"));
    }

    fn locale_path(lang: &str) -> String {
        format!("{}/locales/{lang}.yml", env!("CARGO_MANIFEST_DIR",))
    }

    fn flatten(prefix: &str, v: &serde_norway::Value, out: &mut BTreeMap<String, String>) {
        match v {
            serde_norway::Value::Mapping(map) => {
                for (k, sub) in map {
                    let key = k
                        .as_str()
                        .unwrap_or_else(|| panic!("non-string locale key under {prefix:?}"));
                    let path = if prefix.is_empty() {
                        key.to_string()
                    } else {
                        format!("{prefix}.{key}")
                    };
                    flatten(&path, sub, out);
                }
            }
            serde_norway::Value::String(s) => {
                out.insert(prefix.to_string(), s.clone());
            }
            other => panic!("unexpected locale value at {prefix:?}: {other:?}"),
        }
    }

    fn load(lang: &str) -> BTreeMap<String, String> {
        let text = std::fs::read_to_string(locale_path(lang))
            .unwrap_or_else(|e| panic!("{}: {e}", locale_path(lang)));
        let value: serde_norway::Value = serde_norway::from_str(&text).expect(lang);
        let mut out = BTreeMap::new();
        flatten("", &value, &mut out);
        out
    }

    /// The `%{name}` interpolation slots used by one string.
    fn placeholders(s: &str) -> BTreeSet<String> {
        let mut out = BTreeSet::new();
        let mut rest = s;
        while let Some(i) = rest.find("%{") {
            let after = &rest[i + 2..];
            match after.find('}') {
                Some(end) => {
                    out.insert(after[..end].to_string());
                    rest = &after[end + 1..];
                }
                None => break,
            }
        }
        out
    }

    #[test]
    fn zh_cn_keys_match_en() {
        let en = load("en");
        let zh = load("zh-CN");
        let missing: Vec<&String> = en.keys().filter(|k| !zh.contains_key(*k)).collect();
        let extra: Vec<&String> = zh.keys().filter(|k| !en.contains_key(*k)).collect();
        assert!(
            missing.is_empty() && extra.is_empty(),
            "missing in zh-CN: {missing:?}\nextra beyond en: {extra:?}"
        );
    }

    #[test]
    fn interpolation_slots_match_en_vs_zh_cn() {
        let en = load("en");
        let zh = load("zh-CN");
        for (key, en_text) in &en {
            if let Some(zh_text) = zh.get(key) {
                assert_eq!(
                    placeholders(en_text),
                    placeholders(zh_text),
                    "placeholder mismatch at {key:?}"
                );
            }
        }
    }
}
