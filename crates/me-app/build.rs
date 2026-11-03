fn main() {
    println!("cargo:rustc-check-cfg=cfg(has_cjk_asset)");
    let asset = std::path::Path::new("../../assets/fonts/cjk-subset.ttf");
    println!("cargo:rerun-if-changed={}", asset.display());
    if asset.is_file() {
        println!("cargo:rustc-cfg=has_cjk_asset");
    } else {
        println!(
            "cargo:warning=assets/fonts/cjk-subset.ttf not found; falling back to system CJK fonts at runtime"
        );
    }
}
