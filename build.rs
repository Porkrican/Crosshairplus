use std::path::Path;

const MANIFEST: &str = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<assembly xmlns="urn:schemas-microsoft-com:asm.v1" manifestVersion="1.0">
  <assemblyIdentity type="win32" name="CrosshairPlus" version="1.0.0.0" processorArchitecture="amd64"/>
  <trustInfo xmlns="urn:schemas-microsoft-com:asm.v3">
    <security>
      <requestedPrivileges>
        <requestedExecutionLevel level="asInvoker" uiAccess="false"/>
      </requestedPrivileges>
    </security>
  </trustInfo>
  <compatibility xmlns="urn:schemas-microsoft-com:compatibility.v1">
    <application>
      <supportedOS Id="{8e0f7a12-bfb3-4fe8-b9a5-48fd50a15a9a}"/>
      <supportedOS Id="{4f476546-9370-4f01-9d87-8c97a7fa9f04}"/>
    </application>
  </compatibility>
  <application xmlns="urn:schemas-microsoft-com:asm.v3">
    <windowsSettings>
      <dpiAware xmlns="http://schemas.microsoft.com/SMI/2005/WindowsSettings">true/pm</dpiAware>
      <dpiAwareness xmlns="http://schemas.microsoft.com/SMI/2016/WindowsSettings">PerMonitorV2</dpiAwareness>
      <longPathAware xmlns="http://schemas.microsoft.com/SMI/2016/WindowsSettings">true</longPathAware>
    </windowsSettings>
  </application>
</assembly>
"#;

fn main() {
    println!("cargo:rerun-if-changed=assets/icon.ico");
    println!("cargo:rerun-if-changed=ui");
    println!("cargo:rerun-if-changed=assets/scene_day.png");
    println!("cargo:rerun-if-changed=assets/scene_night.png");
    println!("cargo:rerun-if-changed=assets/presets");

    let mut res = winresource::WindowsResource::new();
    res.set_manifest(MANIFEST);
    res.set("ProductName", "Crosshair+");
    res.set("FileDescription", "Crosshair+ precision desktop suite");
    res.set("LegalCopyright", "MIT licensed");
    res.set("OriginalFilename", "CrosshairPlus.exe");
    if Path::new("assets/icon.ico").exists() {
        res.set_icon("assets/icon.ico");
    } else {
        println!("cargo:warning=assets/icon.ico missing — run `cargo run --bin gen_assets`");
    }
    if let Err(err) = res.compile() {
        panic!("failed to compile Windows resources: {err}");
    }
}
