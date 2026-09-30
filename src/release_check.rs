//! Manual installation updates for Linux and Windows; macOS uses Sparkle.
use gpui_kit::component::{WindowExt as _, button::Button, notification::Notification};
use gpui_kit::*;
use serde::{Deserialize, Serialize};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

const DAY: u64 = 86_400;
const REPO: &str = "https://github.com/alpcanaydin/tusk";
#[derive(Default, Serialize, Deserialize)]
struct Record {
    last_attempt: u64,
    notified: Option<String>,
}
#[derive(Default)]
struct Checking(bool);
impl Global for Checking {}
#[derive(Deserialize)]
struct Release {
    tag_name: String,
    draft: bool,
    prerelease: bool,
    assets: Vec<Asset>,
}
#[derive(Deserialize)]
struct Asset {
    name: String,
}
fn path() -> std::path::PathBuf {
    crate::db::app_dir().join("update-check.json")
}
fn record() -> Record {
    std::fs::read(path())
        .ok()
        .and_then(|s| serde_json::from_slice(&s).ok())
        .unwrap_or_default()
}
fn save(r: &Record) {
    let p = path();
    if let Some(parent) = p.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    if let Ok(s) = serde_json::to_vec(r) {
        let _ = std::fs::write(p, s);
    }
}
fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}
fn candidate(r: Release, current: &str, os: &str, arch: &str) -> Result<Option<String>, String> {
    if r.draft || r.prerelease {
        return Ok(None);
    }
    let version = r.tag_name.strip_prefix('v').unwrap_or(&r.tag_name);
    let next = semver::Version::parse(version)
        .map_err(|_| "Release has an invalid version.".to_string())?;
    let current = semver::Version::parse(current).map_err(|e| e.to_string())?;
    if !next.pre.is_empty() || next <= current {
        return Ok(None);
    }
    let available = r.assets.iter().any(|a| match (os, arch) {
        ("windows", "x86_64") => a.name.ends_with("-windows-x64.zip"),
        ("windows", "aarch64") => a.name.ends_with("-windows-arm64.zip"),
        ("linux", "x86_64") => {
            a.name.ends_with("-ubuntu-amd64.deb")
                || a.name.ends_with("-fedora-x86_64.rpm")
                || a.name.ends_with("-arch-x86_64.pkg.tar.zst")
        }
        _ => false,
    });
    Ok(available.then(|| next.to_string()))
}
fn notify(cx: &mut App, message: String, version: Option<String>) {
    for w in cx.windows() {
        let message = message.clone();
        let version = version.clone();
        let _ = w.update(cx, |_, window, cx| {
            let mut note = Notification::new().message(message);
            if let Some(v) = version {
                note = note.autohide(false).action(move |_, _, _| {
                    let url = format!("{REPO}/releases/tag/v{v}");
                    Button::new("download-release")
                        .label("View release and downloads")
                        .on_click(move |_, _, cx| cx.open_url(&url))
                });
            }
            window.defer(cx, move |window, cx| {
                window.push_notification(note, cx);
            });
        });
    }
}
pub fn init(cx: &mut App) {
    cx.set_global(Checking::default());
    cx.spawn(async |cx: &mut AsyncApp| {
        loop {
            cx.update(|cx| check(cx, false));
            cx.background_executor()
                .timer(Duration::from_secs(60))
                .await;
        }
    })
    .detach();
}
pub fn check(cx: &mut App, manual: bool) {
    if cx.default_global::<Checking>().0 {
        return;
    }
    let mut r = record();
    if !manual
        && (!crate::settings::get().automatic_update_checks
            || now().saturating_sub(r.last_attempt) < DAY)
    {
        return;
    }
    r.last_attempt = now();
    save(&r);
    cx.global_mut::<Checking>().0 = true;
    if manual {
        notify(cx, "Checking for updates…".into(), None);
    }
    let task = crate::db::runtime().spawn(async {
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(10))
            .user_agent(concat!("Tusk/", env!("CARGO_PKG_VERSION")))
            .build()
            .map_err(|e| e.to_string())?;
        let r = client
            .get("https://api.github.com/repos/alpcanaydin/tusk/releases/latest")
            .header("Accept", "application/vnd.github+json")
            .send()
            .await
            .map_err(|e| e.to_string())?
            .error_for_status()
            .map_err(|e| e.to_string())?
            .json::<Release>()
            .await
            .map_err(|e| e.to_string())?;
        candidate(
            r,
            env!("CARGO_PKG_VERSION"),
            std::env::consts::OS,
            std::env::consts::ARCH,
        )
    });
    cx.spawn(async move |cx: &mut AsyncApp| {
        let result = task.await.unwrap_or_else(|e| Err(e.to_string()));
        cx.update(|cx| {
            cx.global_mut::<Checking>().0 = false;
            match result {
                Ok(Some(v)) => {
                    if manual || r.notified.as_ref() != Some(&v) {
                        notify(cx, format!("Tusk {v} is available."), Some(v.clone()));
                        r.notified = Some(v);
                        save(&r);
                    }
                }
                Ok(None) if manual => notify(
                    cx,
                    "No newer release is available for this platform.".into(),
                    None,
                ),
                Err(e) if manual => notify(cx, format!("Could not check for updates: {e}"), None),
                _ => {}
            }
        });
    })
    .detach();
}
#[cfg(test)]
mod tests {
    use super::{Asset, Release, candidate};
    fn release(v: &str, asset: &str) -> Release {
        Release {
            tag_name: v.into(),
            draft: false,
            prerelease: false,
            assets: vec![Asset { name: asset.into() }],
        }
    }
    #[test]
    fn stable_matching_asset_only() {
        assert_eq!(
            candidate(
                release("v0.2.10", "Tusk-0.2.10-windows-x64.zip"),
                "0.2.9",
                "windows",
                "x86_64"
            )
            .unwrap(),
            Some("0.2.10".into())
        );
        assert_eq!(
            candidate(
                release("v0.3.0", "Tusk-windows-arm64.zip"),
                "0.2.1",
                "windows",
                "x86_64"
            )
            .unwrap(),
            None
        );
        assert_eq!(
            candidate(
                release("v0.3.0-beta.1", "Tusk-windows-x64.zip"),
                "0.2.1",
                "windows",
                "x86_64"
            )
            .unwrap(),
            None
        );
        assert!(candidate(release("bad", ""), "0.2.1", "linux", "x86_64").is_err());
    }
}
