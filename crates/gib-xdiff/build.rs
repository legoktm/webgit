//! Builds the vendored xdiff in `vendor/xdiff`.

use std::path::PathBuf;

/// The translation units xdiff needs to link.
const SOURCES: &[&str] = &[
    "xdiffi.c",
    "xprepare.c",
    "xutils.c",
    "xemit.c",
    "xhistogram.c",
    "xpatience.c",
];

fn main() {
    let manifest = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").unwrap());
    let vendor = manifest.join("../../vendor/xdiff");
    let shim = manifest.join("shim/git-xdiff.h");

    if !vendor.join("xdiffi.c").exists() {
        panic!(
            "vendor/xdiff is empty — the submodule has not been checked out.\n\
             Run: git submodule update --init vendor/xdiff"
        );
    }

    let mut build = cc::Build::new();
    build
        .include(&vendor)
        .flag("-include")
        .flag(shim.to_str().expect("shim path is not UTF-8"))
        .flag_if_supported("-ffreestanding")
        .warnings(false);

    for source in SOURCES {
        build.file(vendor.join(source));
    }
    build.compile("gib_xdiff");

    println!("cargo:rerun-if-changed={}", shim.display());
    for source in SOURCES {
        println!("cargo:rerun-if-changed={}", vendor.join(source).display());
    }
    println!(
        "cargo:rerun-if-changed={}",
        vendor.join("xdiff.h").display()
    );
}
