fn main() {
    let config = slint_build::CompilerConfiguration::new()
        // Use one widget style on every host instead of Slint's platform default.
        .with_style("fluent".into())
        .with_debug_info(std::env::var("PROFILE").as_deref() == Ok("debug"));
    slint_build::compile_with_config("ui/player.slint", config).unwrap();
}
