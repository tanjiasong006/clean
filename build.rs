use std::path::PathBuf;

fn main() {
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("windows") {
        return;
    }

    let icon = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").unwrap())
        .join("packaging/icons/clean.ico");
    println!("cargo:rerun-if-changed={}", icon.display());

    // RC parses backslashes as escapes, so a workspace like D:\a\... would lose the "\a".
    let icon_path = icon.display().to_string().replace('\\', "\\\\");
    let rc = PathBuf::from(std::env::var("OUT_DIR").unwrap()).join("clean.rc");
    std::fs::write(&rc, format!("1 ICON \"{icon_path}\"\n")).unwrap();

    embed_resource::compile(&rc, embed_resource::NONE)
        .manifest_optional()
        .expect("failed to embed the application icon");
}
