fn main() {
    let lilv = pkg_config::Config::new().probe("lilv-0").unwrap();
    let suil = pkg_config::Config::new().probe("suil-0").unwrap();
    let x11 = pkg_config::Config::new().probe("x11").unwrap();
    let gl = pkg_config::Config::new().probe("gl").unwrap();

    let mut build = cc::Build::new();
    build.file("src/ui_host.c");
    build.warnings(false);
    for path in lilv
        .include_paths
        .iter()
        .chain(suil.include_paths.iter())
        .chain(x11.include_paths.iter())
        .chain(gl.include_paths.iter())
    {
        build.include(path);
    }
    build.compile("vibemix_ui_host");

    println!("cargo:rustc-link-lib=dl");
    println!("cargo:rerun-if-changed=src/ui_host.c");
}
