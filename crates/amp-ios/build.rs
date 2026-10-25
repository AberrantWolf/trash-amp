fn main() {
    let manifest = std::path::PathBuf::from(std::env::var_os("CARGO_MANIFEST_DIR").unwrap());
    let entitlements = manifest.join("../../assets/ios/Simulator.entitlements");
    println!("cargo:rerun-if-changed={}", entitlements.display());
    let ios = std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("ios");
    let simulator = std::env::var("CARGO_CFG_TARGET_ABI").as_deref() == Ok("sim")
        || std::env::var("CARGO_CFG_TARGET_ARCH").as_deref() == Ok("x86_64");
    if ios && simulator {
        // Match Xcode's simulator linker behavior. securityd reads this section;
        // the executable itself retains an ordinary ad-hoc signature.
        println!(
            "cargo:rustc-link-arg=-Wl,-sectcreate,__TEXT,__entitlements,{}",
            entitlements.display()
        );
    }
}
