use std::path::PathBuf;

fn main() {
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("windows") {
        return;
    }

    let icon = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").unwrap())
        .join("packaging/icons/clean.ico");
    println!("cargo:rerun-if-changed={}", icon.display());

    let rc = PathBuf::from(std::env::var("OUT_DIR").unwrap()).join("clean.rc");
    std::fs::write(&rc, format!("1 ICON \"{}\"\n", icon.display())).unwrap();

    embed_resource::compile(&rc, embed_resource::NONE)
        .manifest_optional()
        .expect("failed to embed the application icon");
}
