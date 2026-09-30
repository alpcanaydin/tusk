// Embed an Info.plist in the bare binary so a `cargo run` build is named
// "Tusk" in the menu bar (Tusk.app has its own Info.plist). Name keys only:
// a bundle identifier would make the notification center expect a real bundle.
fn main() {
    // Windows: the app icon, embedded as the exe's first icon resource (the
    // taskbar, Explorer and window title bar use it).
    #[cfg(windows)]
    {
        let ico = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("assets/windows/tusk.ico");
        println!("cargo:rerun-if-changed={}", ico.display());
        let mut res = winresource::WindowsResource::new();
        res.set_icon(ico.to_str().expect("icon path"));
        res.set("ProductName", "Tusk");
        res.set("FileDescription", "Tusk");
        res.compile().expect("embed the Windows icon");
    }
    let plist = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("assets/dev-Info.plist");
    println!("cargo:rerun-if-changed={}", plist.display());
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("macos") {
        println!(
            "cargo:rustc-link-arg-bins=-Wl,-sectcreate,__TEXT,__info_plist,{}",
            plist.display()
        );
    }
}
