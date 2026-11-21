fn main() -> std::io::Result<()> {
    println!("cargo:rerun-if-changed=packaging/synth.ico");
    println!("cargo:rerun-if-env-changed=SYNTH_VERSION");
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        let version =
            std::env::var("SYNTH_VERSION").unwrap_or_else(|_| env!("CARGO_PKG_VERSION").to_owned());
        let components: Result<Vec<u16>, _> = version.split('.').map(str::parse).collect();
        let [major, minor, patch] = components
            .ok()
            .and_then(|components| components.try_into().ok())
            .ok_or_else(|| std::io::Error::other(format!("Invalid Windows version: {version}")))?;
        let numeric = u64::from(major) << 48 | u64::from(minor) << 32 | u64::from(patch) << 16;
        winresource::WindowsResource::new()
            .set_icon("packaging/synth.ico")
            .set("FileVersion", &version)
            .set("ProductVersion", &version)
            .set_version_info(winresource::VersionInfo::FILEVERSION, numeric)
            .set_version_info(winresource::VersionInfo::PRODUCTVERSION, numeric)
            .compile()?;
    }
    Ok(())
}
