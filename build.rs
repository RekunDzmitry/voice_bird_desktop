use std::env;

fn main() {
    // Only run this for macOS builds
    if env::var("CARGO_CFG_TARGET_OS").unwrap() == "macos" {
        // Add rpath to find Swift runtime libraries
        println!("cargo:rustc-link-arg=-Wl,-rpath,/usr/lib/swift");
        println!("cargo:rustc-link-arg=-Wl,-rpath,@executable_path/../Frameworks");
        println!("cargo:rustc-link-arg=-Wl,-rpath,@loader_path/../Frameworks");
    }

    tauri_build::build()
}
