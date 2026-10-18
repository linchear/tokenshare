// Embed the app icon into the Windows executable. Runs for any host, so it keys
// off the target, which also covers cross-builds with mingw-w64.
fn main() {
    println!("cargo:rerun-if-changed=icon.rc");
    println!("cargo:rerun-if-changed=icon.ico");
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        embed_resource::compile("icon.rc", embed_resource::NONE)
            .manifest_required()
            .unwrap();
    }
}
