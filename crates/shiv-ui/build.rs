fn main() -> Result<(), slint_build::CompileError> {
    // Fluent is used only for the ListView scroll bar; all other controls are custom.
    // Debug builds (and so tests) carry element debug info for the testing
    // backend's ElementHandle API; release builds do not.
    let debug = std::env::var("PROFILE").is_ok_and(|p| p == "debug");
    let config = slint_build::CompilerConfiguration::new()
        .with_style("fluent-dark".into())
        .with_debug_info(debug);
    slint_build::compile_with_config("ui/app.slint", config)?;

    #[cfg(windows)]
    {
        let mut res = winresource::WindowsResource::new();
        res.set_icon("../../packaging/windows/shiv.ico");
        res.set("ProductName", "Shiv");
        res.set("FileDescription", "Shiv, Deadlock mod manager");
        res.compile()
            .expect("embedding the Windows resources failed");
    }
    Ok(())
}
