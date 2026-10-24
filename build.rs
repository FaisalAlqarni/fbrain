fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    // ort's prebuilt ONNX Runtime calls ___isPlatformVersionAtLeast (clang's `@available`), which
    // lives in clang's runtime library; rustc's macOS link does not include it.
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("macos") {
        return;
    }
    let dir = std::process::Command::new("xcrun")
        .args(["clang", "--print-resource-dir"])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string());
    match dir {
        Some(dir) => {
            println!("cargo:rustc-link-search=native={dir}/lib/darwin");
            println!("cargo:rustc-link-lib=static=clang_rt.osx");
        }
        None => {
            println!("cargo:warning=xcrun clang --print-resource-dir failed; linking ort may fail")
        }
    }
}
