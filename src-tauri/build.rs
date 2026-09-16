fn main() {
    tauri_build::build();
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("macos") {
        println!("cargo:rustc-link-lib=framework=ScreenCaptureKit");
        println!("cargo:rustc-link-lib=framework=CoreGraphics");
        println!("cargo:rustc-link-lib=framework=AppKit");
        println!("cargo:rerun-if-changed=src/capture/macos_sck.m");
        cc::Build::new()
            .file("src/capture/macos_sck.m")
            .flag("-fobjc-arc")
            .compile("cropmark_sck");
    }
}
