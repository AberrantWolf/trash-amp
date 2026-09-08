fn main() {
    let config = slint_build::CompilerConfiguration::new()
        .with_debug_info(std::env::var("PROFILE").as_deref() == Ok("debug"));
    slint_build::compile_with_config("ui/player.slint", config).unwrap();
}
