fn main() {
    // A debug (development) build gets the amber icon, so it can be told from the normal build in the taskbar and the
    // Alt+Tab list. Regenerate both with `node tools/make-icons.mjs`.
    if std::env::var("PROFILE").as_deref() == Ok("debug") {
        let windows = tauri_build::WindowsAttributes::new().window_icon_path("icons/icon-dev.ico");
        tauri_build::try_build(tauri_build::Attributes::new().windows_attributes(windows)).expect("failed to run tauri-build");
    } else {
        tauri_build::build();
    }
    println!("cargo:rerun-if-changed=icons/icon-dev.ico");
    println!("cargo:rerun-if-changed=icons/icon.ico");
}
