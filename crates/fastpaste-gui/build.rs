// `slint_build::compile()` writes a single generated module and points the
// `SLINT_INCLUDE_GENERATED` env var at it; calling it twice would make the
// second call overwrite the first, leaving only one component visible to
// `slint::include_modules!()`. So instead we compile a single root file —
// `ui/main.slint` — which imports and re-exports every component and struct
// from the other `.slint` files. (SelectionDialog lives in
// `selection_dialog.slint`, MainWindow in `main_window.slint`.)
//
// The style is pinned to `fluent` so the few std-widgets still in use
// (TextEdit, ListView scrollbars) render identically on every machine —
// without this, the `SLINT_STYLE` env var could silently swap the look.
// The rest of the UI is the in-house compact set from `ui/widgets.slint`,
// which is style-independent.
fn main() -> Result<(), Box<dyn std::error::Error>> {
    let config = slint_build::CompilerConfiguration::new().with_style("fluent".into());
    slint_build::compile_with_config("ui/main.slint", config)?;
    windows_resource()?;
    Ok(())
}

/// The icon Explorer and the taskbar show for the exe, and the version and
/// name its Properties and Task Manager show. The version comes from the
/// workspace, like everything else's.
///
/// A build script is compiled for the host, so `cfg(windows)` here would
/// ask about the wrong machine; Cargo passes the target in the
/// environment instead.
fn windows_resource() -> std::io::Result<()> {
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("windows") {
        return Ok(());
    }
    const ICON: &str = "../../packaging/windows/fastpaste.ico";
    println!("cargo:rerun-if-changed={ICON}");
    winresource::WindowsResource::new()
        .set_icon(ICON)
        .set("ProductName", "fastpaste")
        .set("FileDescription", "fastpaste")
        .set("InternalName", "fastpaste")
        .set("OriginalFilename", "fastpaste.exe")
        .set("LegalCopyright", "Copyright (c) 2026 The fastpaste authors")
        .compile()
}
