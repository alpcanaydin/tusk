//! Dock icon for the bare dev binary (`cargo run` isn't an .app bundle, so
//! macOS would show a generic icon). A bundled Tusk.app keeps its compiled
//! Icon Composer icon instead.

#[cfg(target_os = "macos")]
pub fn set_icon() {
    use objc2::AnyThread as _;
    use objc2::MainThreadMarker;
    use objc2_app_kit::{NSApplication, NSImage};
    use objc2_foundation::NSData;

    let in_bundle =
        std::env::current_exe().is_ok_and(|p| p.to_string_lossy().contains(".app/Contents/MacOS/"));
    if in_bundle {
        return;
    }
    let Some(mtm) = MainThreadMarker::new() else {
        return;
    };
    let data = NSData::with_bytes(include_bytes!("../assets/icon/tusk-1024.png"));
    let Some(image) = NSImage::initWithData(NSImage::alloc(), &data) else {
        return;
    };
    let app = NSApplication::sharedApplication(mtm);
    unsafe { app.setApplicationIconImage(Some(&image)) };
}

#[cfg(not(target_os = "macos"))]
pub fn set_icon() {}

/// The bold app-menu title comes from the process name; the dev binary is
/// `tusk`, the bundle says "Tusk" (CFBundleName). Call before the app runs.
#[cfg(target_os = "macos")]
pub fn set_process_name() {
    use objc2_foundation::{NSProcessInfo, NSString};
    NSProcessInfo::processInfo().setProcessName(&NSString::from_str("Tusk"));
}

#[cfg(not(target_os = "macos"))]
pub fn set_process_name() {}
