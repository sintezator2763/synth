fn main() -> std::io::Result<()> {
    println!("cargo:rerun-if-changed=packaging/synth.ico");
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        winresource::WindowsResource::new()
            .set_icon("packaging/synth.ico")
            .compile()?;
    }
    Ok(())
}
