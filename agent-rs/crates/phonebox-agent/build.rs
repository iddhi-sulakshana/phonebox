//! Windows: the exe's icon, version information and manifest (C#'s `ApplicationIcon` and assembly attributes). They
//! need Windows' resource compiler, so only a Windows build on Windows gets them; the test server's Windows clippy
//! (`scripts/remote-cross-check.sh`, on Linux) skips them.

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=../../Cargo.toml");
    #[cfg(windows)]
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        resources();
    }
}

/// `asInvoker`, the common controls of the C# build (visual styles for the warning box), and per-monitor DPI so the
/// tray menu is drawn sharp; Windows 10 and later.
#[cfg(windows)]
const MANIFEST: &str = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<assembly xmlns="urn:schemas-microsoft-com:asm.v1" manifestVersion="1.0">
  <trustInfo xmlns="urn:schemas-microsoft-com:asm.v3">
    <security>
      <requestedPrivileges>
        <requestedExecutionLevel level="asInvoker" uiAccess="false"/>
      </requestedPrivileges>
    </security>
  </trustInfo>
  <dependency>
    <dependentAssembly>
      <assemblyIdentity type="win32" name="Microsoft.Windows.Common-Controls" version="6.0.0.0"
        processorArchitecture="*" publicKeyToken="6595b64144ccf1df" language="*"/>
    </dependentAssembly>
  </dependency>
  <application xmlns="urn:schemas-microsoft-com:asm.v3">
    <windowsSettings>
      <dpiAware xmlns="http://schemas.microsoft.com/SMI/2005/WindowsSettings">true/pm</dpiAware>
      <dpiAwareness xmlns="http://schemas.microsoft.com/SMI/2016/WindowsSettings">PerMonitorV2</dpiAwareness>
    </windowsSettings>
  </application>
  <compatibility xmlns="urn:schemas-microsoft-com:compatibility.v1">
    <application>
      <supportedOS Id="{8e0f7a12-bfb3-4fe8-b9a5-48fd50a15a9a}"/>
    </application>
  </compatibility>
</assembly>
"#;

#[cfg(windows)]
fn resources() {
    let env = |name: &str| std::env::var(name).unwrap_or_default();
    let version = env("CARGO_PKG_VERSION");
    // the numeric version cannot hold "alpha": 1.0.0-alpha.3 is 1.0.0.3, a final 1.0.0 is 1.0.0.0
    let part = |name: &str| env(name).parse::<u64>().unwrap_or(0) & 0xffff;
    let build = env("CARGO_PKG_VERSION_PRE")
        .rsplit('.')
        .next()
        .and_then(|n| n.parse::<u64>().ok())
        .unwrap_or(0)
        & 0xffff;
    let numeric = part("CARGO_PKG_VERSION_MAJOR") << 48
        | part("CARGO_PKG_VERSION_MINOR") << 32
        | part("CARGO_PKG_VERSION_PATCH") << 16
        | build;
    let icon = "../../../agent/PhoneBox.Agent/phonebox.ico";
    println!("cargo:rerun-if-changed={icon}");
    let mut res = winresource::WindowsResource::new();
    res.set_icon(icon)
        .set("ProductName", "PhoneBox")
        .set("CompanyName", "PhoneBox")
        .set(
            "FileDescription",
            "PhoneBox desktop agent: feeds the PhoneBox Android desk panel over USB or Wi-Fi.",
        )
        .set("ProductVersion", &version)
        .set("FileVersion", &version)
        .set("OriginalFilename", "PhoneBoxAgent.exe")
        .set_version_info(winresource::VersionInfo::FILEVERSION, numeric)
        .set_version_info(winresource::VersionInfo::PRODUCTVERSION, numeric)
        .set_manifest(MANIFEST);
    res.compile()
        .expect("the Windows resources (icon, version, manifest)");
}
