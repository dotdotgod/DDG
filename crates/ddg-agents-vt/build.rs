fn main() {
    println!("cargo:rerun-if-env-changed=GHOSTTY_VT_LIB_DIR");
    let dir = std::env::var("GHOSTTY_VT_LIB_DIR")
        .expect("set GHOSTTY_VT_LIB_DIR to the Ghostty 1.0.1 library directory");
    println!("cargo:rustc-link-search=native={dir}");
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        println!("cargo:rerun-if-env-changed=GHOSTTY_VT_LINK_MODE");
        let archive = std::path::Path::new(&dir).join("ghostty-vt-static.lib");
        println!("cargo:rerun-if-changed={}", archive.display());
        if std::env::var("GHOSTTY_VT_LINK_MODE").as_deref() == Ok("dylib") {
            println!("cargo:rustc-link-lib=dylib=ghostty-vt");
            return;
        }
        let name = if archive.is_file() {
            "ghostty-vt-static"
        } else {
            "ghostty-vt"
        };
        println!("cargo:rustc-link-lib=static={name}");
        if std::env::var("CARGO_CFG_TARGET_ENV").as_deref() == Ok("msvc") {
            println!("cargo:rustc-link-lib=ntdll");
        }
    } else {
        println!("cargo:rustc-link-lib=static=ghostty-vt");
    }
}
