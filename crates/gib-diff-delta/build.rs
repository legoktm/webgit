//! Builds git's vendored delta encoder in `vendor/git/diff-delta.c`.

use std::path::PathBuf;

fn main() {
    let manifest = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").unwrap());
    let source = manifest.join("../../vendor/git/diff-delta.c");
    let shim = manifest.join("shim");

    cc::Build::new()
        .file(&source)
        .include(&shim)
        .flag_if_supported("-ffreestanding")
        .warnings(false)
        .compile("gib_diff_delta");

    println!("cargo:rerun-if-changed={}", source.display());
    for header in ["git-compat-util.h", "delta.h"] {
        println!("cargo:rerun-if-changed={}", shim.join(header).display());
    }
}
