// Embeds the Windows application icon and version metadata into the exe, so
// Explorer, the taskbar, Alt-Tab and the tray all show the real icon rather
// than the generic default.
//
// Keyed on the *target* OS rather than `#[cfg(windows)]`, which describes the
// machine running this script: under that test a cross-compiled build quietly
// shipped with no icon at all. winresource finds the matching `windres` itself
// (x86_64-w64-mingw32-windres for a mingw cross build, rc.exe under MSVC).
fn main() {
    println!("cargo:rerun-if-changed=assets/lyricarr.ico");
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("windows") {
        return;
    }
    let mut res = winresource::WindowsResource::new();
    res.set_icon("assets/lyricarr.ico");
    res.set("ProductName", "Lyricarr");
    res.set("FileDescription", "Fetch synced lyrics from LRCLIB into .lrc sidecars");
    res.set("LegalCopyright", "");
    if let Err(e) = res.compile() {
        // Not fatal: a missing rc.exe should degrade to "no icon", never to
        // "cannot build".
        println!("cargo:warning=icon not embedded: {e}");
    }
}
