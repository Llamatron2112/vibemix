// Compiles the CLAP host shim against the vendored CLAP C headers.

use std::path::PathBuf;

fn main() {
    let manifest = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").unwrap());
    let include = manifest.join("../../third_party/clap/include");
    let include = include.canonicalize().unwrap_or(include);
    assert!(
        include.join("clap/clap.h").is_file(),
        "CLAP headers missing at {}; run `git submodule update --init --recursive third_party/clap`",
        include.display()
    );

    let mut build = cc::Build::new();
    build
        .cpp(true)
        .std("c++17")
        .include(&include)
        .warnings(false)
        .flag("-fPIC")
        // Advertise the host version to plugins (clap_host.version) from the
        // crate version, so it can never drift from the release.
        .define(
            "VIBEMIX_HOST_VERSION",
            Some(format!("\"{}\"", env!("CARGO_PKG_VERSION")).as_str()),
        );
    build.file(manifest.join("src/shim.cpp"));
    build.compile("vibemix_clap_shim");

    println!("cargo:rustc-link-lib=dylib=stdc++");
    println!("cargo:rustc-link-lib=dylib=dl");
    println!("cargo:rustc-link-lib=dylib=pthread");
    println!("cargo:rustc-link-lib=dylib=m");
    // The editor window is embedded into a raw X11 window.
    println!("cargo:rustc-link-lib=dylib=X11");

    println!("cargo:rerun-if-changed=src/shim.cpp");
    println!("cargo:rerun-if-env-changed=CARGO_PKG_VERSION");
}
