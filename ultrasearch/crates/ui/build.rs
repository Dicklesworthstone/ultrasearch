// Embed the UltraSearch icon in ui.exe, so Explorer and the taskbar show it
// instead of a blank page (#9). winres drives rc.exe from the Windows SDK, so
// only a Windows host building for Windows embeds it.

fn main() {
    println!("cargo:rerun-if-changed=../../assets/icon.ico");
    if !cfg!(windows) || std::env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("windows") {
        return;
    }
    let mut res = winres::WindowsResource::new();
    res.set_icon("../../assets/icon.ico");
    if let Err(err) = res.compile() {
        println!("cargo:warning=ui.exe will have no icon: {err}");
    }
}
