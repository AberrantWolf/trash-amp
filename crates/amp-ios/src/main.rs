#[cfg(target_os = "ios")]
mod credentials;
#[cfg(target_os = "ios")]
mod host;

#[cfg(target_os = "ios")]
fn main() -> anyhow::Result<()> {
    host::run()
}

#[cfg(not(target_os = "ios"))]
fn main() {
    eprintln!("Build trash-amp-ios for aarch64-apple-ios or aarch64-apple-ios-sim.");
    std::process::exit(1);
}
