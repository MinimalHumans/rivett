fn main() {
    // Tell Cargo to rerun this script if the icon or the build script itself changes
    println!("cargo:rerun-if-changed=resources/icon.ico");
    println!("cargo:rerun-if-changed=resources/icon.png");
    println!("cargo:rerun-if-changed=build.rs");

    if std::env::var("CARGO_CFG_TARGET_OS").unwrap() == "windows" {
        let mut res = winres::WindowsResource::new();
        res.set_icon("resources/icon.ico");
        // Declare PerMonitorV2 DPI awareness so Windows never virtualises the
        // window; without this, popup/context-menu surfaces are blurry on
        // high-DPI displays because the OS applies bitmap scaling.
        res.set_manifest(r#"
<assembly xmlns="urn:schemas-microsoft-com:asm.v1" manifestVersion="1.0" xmlns:asmv3="urn:schemas-microsoft-com:asm.v3">
  <asmv3:application>
    <asmv3:windowsSettings>
      <dpiAware xmlns="http://schemas.microsoft.com/SMI/2005/WindowsSettings">true/PM</dpiAware>
      <dpiAwareness xmlns="http://schemas.microsoft.com/SMI/2016/WindowsSettings">PerMonitorV2, PerMonitor</dpiAwareness>
    </asmv3:windowsSettings>
  </asmv3:application>
</assembly>
"#);
        res.compile().unwrap();

        // Force NvOptimusEnablement/AmdPowerXpressRequestHighPerformance (declared
        // in main.rs) into the .exe's export table — MSVC's linker doesn't export
        // symbols from an executable by default, and the GPU drivers look them up
        // there (GetProcAddress on the main module), not just in the symbol table.
        if std::env::var("CARGO_CFG_TARGET_ENV").as_deref() == Ok("msvc") {
            println!("cargo:rustc-link-arg=/EXPORT:NvOptimusEnablement");
            println!("cargo:rustc-link-arg=/EXPORT:AmdPowerXpressRequestHighPerformance");
        }
    }
}
