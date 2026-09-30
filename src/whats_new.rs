//! "Updated to Tusk vX.Y.Z": the first launch after an update (Sparkle,
//! Homebrew or a manual install) shows a toast linking to that version's
//! GitHub release notes. The last version seen is kept in `last_version`
//! in the data folder; a fresh install only records it, a downgrade or an
//! unchanged version shows nothing.

use gpui_kit::component::WindowExt as _;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::notification::Notification;
use gpui_kit::component::{ActiveTheme as _, Icon, Sizable as _, h_flex, v_flex};
use gpui_kit::*;

use crate::theme::TextCaption as _;

const VERSION: &str = env!("CARGO_PKG_VERSION");

/// Release tags are `v<version>` (scripts/tag-release.sh, appcast.sh).
pub fn release_notes_url(version: &str) -> String {
    format!("https://github.com/alpcanaydin/tusk/releases/tag/v{version}")
}

/// Open this version's release notes (menu / palette).
pub fn open_release_notes(cx: &mut App) {
    cx.open_url(&release_notes_url(VERSION));
}

fn last_version_path() -> std::path::PathBuf {
    crate::db::app_dir().join("last_version")
}

/// `1.2.10` → `[1, 2, 10]`; a pre-release suffix (`0.2.0-beta`) is ignored.
fn parse(v: &str) -> Vec<u64> {
    v.trim()
        .split('.')
        .map(|p| {
            let digits: String = p.chars().take_while(char::is_ascii_digit).collect();
            digits.parse().unwrap_or(0)
        })
        .collect()
}

/// Announce only an upgrade from a version seen before.
fn is_upgrade(last: Option<&str>, current: &str) -> bool {
    last.is_some_and(|last| parse(current) > parse(last))
}

/// Record the running version; `Some(version)` when it is an upgrade.
fn take_update() -> Option<String> {
    let path = last_version_path();
    let last = std::fs::read_to_string(&path).ok();
    if last.as_deref().map(str::trim) != Some(VERSION) {
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let _ = std::fs::write(&path, VERSION);
    }
    is_upgrade(last.as_deref(), VERSION).then(|| VERSION.to_string())
}

/// Called once for the first main window of a launch.
pub fn announce(window: &mut Window, cx: &mut App) {
    let Some(version) = take_update() else {
        return;
    };
    // Deferred: the window's root isn't set while it is being built.
    window.defer(cx, move |window, cx| {
        let url = release_notes_url(&version);
        let note = Notification::new()
            .autohide(false)
            .close_visible()
            .content(move |_, _, cx| card(&version, &url, cx));
        window.push_notification(note, cx);
    });
}

/// "What's new" card: label, title, one line, then a full-width button
/// (the kit's action button would squeeze the text into a column).
fn card(version: &str, url: &str, cx: &mut Context<Notification>) -> AnyElement {
    let t = cx.theme();
    let (accent, muted) = (t.accent, t.muted_foreground);
    let url = url.to_string();
    v_flex()
        .w_full()
        .gap_1()
        .font_family(crate::settings::ui_font())
        .child(
            h_flex()
                .gap_1()
                .text_caption()
                .font_weight(FontWeight::MEDIUM)
                .text_color(accent)
                .child(Icon::new(gpui_kit::assets::IconName::Sparkles).size(px(12.)))
                .child("WHAT'S NEW"),
        )
        // Right padding clears the close button in the corner.
        .child(
            div()
                .pr_6()
                .text_sm()
                .font_weight(FontWeight::SEMIBOLD)
                .child(format!("Updated to Tusk {version}")),
        )
        .child(
            div()
                .text_sm()
                .text_color(muted)
                .child("See what changed in this release."),
        )
        .child(
            Button::new("release-notes")
                .primary()
                .small()
                .w_full()
                .mt_2()
                .label("View Release Notes")
                .on_click(cx.listener(move |note, _, window, cx| {
                    cx.open_url(&url);
                    note.dismiss(window, cx);
                })),
        )
        .into_any_element()
}

#[cfg(test)]
mod tests {
    use super::{is_upgrade, release_notes_url};

    #[test]
    fn announces_only_upgrades() {
        assert!(!is_upgrade(None, "0.1.1"), "fresh install");
        assert!(!is_upgrade(Some("0.1.1"), "0.1.1"), "same version");
        assert!(!is_upgrade(Some("0.2.0"), "0.1.1"), "downgrade");
        assert!(is_upgrade(Some("0.1.1"), "0.1.2"));
        assert!(
            is_upgrade(Some("0.1.9\n"), "0.1.10"),
            "numeric, not lexical"
        );
        assert!(is_upgrade(Some("0.9.0"), "1.0.0"));
    }

    #[test]
    fn release_notes_link_uses_the_tag() {
        assert_eq!(
            release_notes_url("0.1.1"),
            "https://github.com/alpcanaydin/tusk/releases/tag/v0.1.1"
        );
    }
}
