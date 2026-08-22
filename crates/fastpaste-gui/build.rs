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
    Ok(())
}
