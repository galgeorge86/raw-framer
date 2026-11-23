//! Embeds the app icon and version info into raw-framer.exe (Explorer, shortcuts, pinned
//! taskbar icon, Properties → Details). The running window's icon is set in main.rs.

fn main() {
    println!("cargo:rerun-if-changed=assets/icon/raw-framer.ico");
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("windows") {
        return;
    }
    let mut res = winresource::WindowsResource::new();
    res.set_icon("assets/icon/raw-framer.ico")
        .set("ProductName", "Raw Framer")
        .set("FileDescription", "Raw Framer")
        .set("LegalCopyright", "Copyright (c) 2026 George Gal")
        .set("OriginalFilename", "raw-framer.exe");
    if let Err(e) = res.compile() {
        // Missing rc.exe shouldn't stop a build; the exe just keeps the default icon.
        println!("cargo:warning=could not embed the app icon: {e}");
    }
}
