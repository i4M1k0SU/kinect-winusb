//! Windows VERSIONINFO shared by the proxy DLL and the tool.

use std::env;

const AUTHOR: &str = "i4M1k0SU";

/// Run executables as the invoker. Without a requested execution level,
/// Windows treats a 32-bit program whose version resource mentions "setup"
/// (as the tool's description does) as an installer and demands elevation.
const MANIFEST: &str = r#"<assembly xmlns="urn:schemas-microsoft-com:asm.v1" manifestVersion="1.0">
<trustInfo xmlns="urn:schemas-microsoft-com:asm.v3">
    <security>
        <requestedPrivileges>
            <requestedExecutionLevel level="asInvoker" uiAccess="false" />
        </requestedPrivileges>
    </security>
</trustInfo>
</assembly>"#;

pub fn compile(filename: &str, dll: bool) {
    println!("cargo:rerun-if-changed=../../build/windows.rs");
    if env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("windows") {
        return;
    }

    let product = env::var("CARGO_PKG_NAME").expect("package name");
    let version = env::var("CARGO_PKG_VERSION").expect("package version");
    let year = jiff::Timestamp::now().to_zoned(jiff::tz::TimeZone::UTC).year();
    let mut numeric = 0u64;
    for component in ["MAJOR", "MINOR", "PATCH"] {
        let value: u16 = env::var(format!("CARGO_PKG_VERSION_{component}"))
            .expect("version component")
            .parse()
            .expect("Windows version components must fit in 0..=65535");
        numeric = (numeric << 16) | u64::from(value);
    }
    numeric <<= 16;

    let mut resource = winresource::WindowsResource::new();
    resource
        .set("CompanyName", AUTHOR)
        .set("ProductName", &product)
        .set("LegalCopyright", &format!("Copyright (c) {year} {AUTHOR}"))
        .set("FileDescription", &env::var("CARGO_PKG_DESCRIPTION").expect("package description"))
        .set("FileVersion", &version)
        .set("ProductVersion", &version)
        .set("InternalName", filename.rsplit_once('.').map_or(filename, |(name, _)| name))
        .set("OriginalFilename", filename)
        .set_version_info(winresource::VersionInfo::FILEVERSION, numeric)
        .set_version_info(winresource::VersionInfo::PRODUCTVERSION, numeric)
        .set_version_info(winresource::VersionInfo::FILETYPE, if dll { 2 } else { 1 });
    if !env::var("CARGO_PKG_VERSION_PRE").expect("prerelease component").is_empty() {
        resource.set_version_info(winresource::VersionInfo::FILEFLAGS, 2); // VS_FF_PRERELEASE
    }
    if !dll {
        resource.set_manifest(MANIFEST);
    }
    resource.compile().expect("compile Windows version resource (Windows SDK / rc.exe required)");
}
