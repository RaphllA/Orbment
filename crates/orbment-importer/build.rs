fn main() {
    println!("cargo:rerun-if-changed=orbment-importer.manifest");
    if std::env::var("CARGO_CFG_TARGET_ENV").as_deref() == Ok("msvc") {
        let manifest =
            std::path::Path::new(&std::env::var("CARGO_MANIFEST_DIR").expect("manifest directory"))
                .join("orbment-importer.manifest");
        println!("cargo:rustc-link-arg-bin=orbment-importer=/MANIFEST:EMBED");
        println!(
            "cargo:rustc-link-arg-bin=orbment-importer=/MANIFESTINPUT:{}",
            manifest.display()
        );
        println!("cargo:rustc-link-arg-bin=orbment-importer=/MANIFESTUAC:NO");
    }
}
