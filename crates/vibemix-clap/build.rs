// Compiles the CLAP host shim against the vendored CLAP C headers.

use std::path::PathBuf;

fn main() {
    let manifest = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").unwrap());
    let include = manifest.join("../../vendor/clap/include");
    let include = include.canonicalize().unwrap_or(include);
    assert!(
        include.join("clap/clap.h").is_file(),
        "CLAP headers missing at {}; run `git submodule update --init --recursive vendor/clap`",
        include.display()
    );

    let mut build = cc::Build::new();
    build
        .cpp(true)
        .std("c++17")
        .include(&include)
        .warnings(false)
        .flag("-fPIC");
    build.file(manifest.join("src/shim.cpp"));
    build.compile("vibemix_clap_shim");

    println!("cargo:rustc-link-lib=dylib=stdc++");
    println!("cargo:rustc-link-lib=dylib=dl");
    println!("cargo:rustc-link-lib=dylib=pthread");
    println!("cargo:rustc-link-lib=dylib=m");
    // The editor window is embedded into a raw X11 window.
    println!("cargo:rustc-link-lib=dylib=X11");

    println!("cargo:rerun-if-changed=src/shim.cpp");
}
