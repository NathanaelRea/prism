fn main() {
    println!("cargo::rerun-if-env-changed=CARGO_CFG_TARGET_OS");

    let target_os = std::env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();
    if !matches!(target_os.as_str(), "linux" | "macos" | "windows") {
        eprintln!(
            "error: unsupported Prism target OS '{target_os}'; Prism supports only Linux, macOS, and Windows"
        );
        std::process::exit(1);
    }
}
