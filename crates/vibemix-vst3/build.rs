// Compiles the VST3 SDK subset (hosting) plus our C shim into a static library,
// and a minimal test effect into a `.vst3` bundle used by the crate's tests.
// We intentionally avoid the SDK's CMake (it pulls in samples/tools we don't
// need).

use std::path::{Path, PathBuf};
use std::process::Command;

fn sdk_base(sdk: &Path) -> Vec<PathBuf> {
    [
        "base/source/baseiids.cpp",
        "base/source/fbuffer.cpp",
        "base/source/fdebug.cpp",
        "base/source/fdynlib.cpp",
        "base/source/fobject.cpp",
        "base/source/fstreamer.cpp",
        "base/source/fstring.cpp",
        "base/source/timer.cpp",
        "base/source/updatehandler.cpp",
        "base/thread/source/fcondition.cpp",
        "base/thread/source/flock.cpp",
    ]
    .iter()
    .map(|p| sdk.join(p))
    .collect()
}

fn sdk_pluginterfaces(sdk: &Path) -> Vec<PathBuf> {
    [
        "pluginterfaces/base/conststringtable.cpp",
        "pluginterfaces/base/coreiids.cpp",
        "pluginterfaces/base/funknown.cpp",
        "pluginterfaces/base/ustring.cpp",
    ]
    .iter()
    .map(|p| sdk.join(p))
    .collect()
}

fn sdk_common(sdk: &Path) -> Vec<PathBuf> {
    [
        "public.sdk/source/common/commoniids.cpp",
        "public.sdk/source/common/commonstringconvert.cpp",
        "public.sdk/source/common/memorystream.cpp",
        "public.sdk/source/common/openurl.cpp",
        "public.sdk/source/common/systemclipboard_linux.cpp",
        "public.sdk/source/common/threadchecker_linux.cpp",
        "public.sdk/source/common/readfile.cpp",
        "public.sdk/source/vst/vstpresetfile.cpp",
    ]
    .iter()
    .map(|p| sdk.join(p))
    .collect()
}

fn sdk_vst(sdk: &Path) -> Vec<PathBuf> {
    [
        "public.sdk/source/common/pluginview.cpp",
        "public.sdk/source/main/pluginfactory.cpp",
        "public.sdk/source/main/moduleinit.cpp",
        "public.sdk/source/vst/utility/dataexchange.cpp",
        "public.sdk/source/vst/utility/stringconvert.cpp",
        "public.sdk/source/vst/utility/systemtime.cpp",
        "public.sdk/source/vst/utility/testing.cpp",
        "public.sdk/source/vst/utility/vst2persistence.cpp",
        "public.sdk/source/vst/vstaudioeffect.cpp",
        "public.sdk/source/vst/vstbus.cpp",
        "public.sdk/source/vst/vstcomponent.cpp",
        "public.sdk/source/vst/vstcomponentbase.cpp",
        "public.sdk/source/vst/vsteditcontroller.cpp",
        "public.sdk/source/vst/vstinitiids.cpp",
        "public.sdk/source/vst/vstnoteexpressiontypes.cpp",
        "public.sdk/source/vst/vstparameters.cpp",
        "public.sdk/source/vst/vstrepresentation.cpp",
    ]
    .iter()
    .map(|p| sdk.join(p))
    .collect()
}

fn sdk_hosting(sdk: &Path) -> Vec<PathBuf> {
    [
        "public.sdk/source/vst/hosting/connectionproxy.cpp",
        "public.sdk/source/vst/hosting/eventlist.cpp",
        "public.sdk/source/vst/hosting/hostclasses.cpp",
        "public.sdk/source/vst/hosting/module.cpp",
        "public.sdk/source/vst/hosting/module_linux.cpp",
        "public.sdk/source/vst/hosting/parameterchanges.cpp",
        "public.sdk/source/vst/hosting/pluginterfacesupport.cpp",
        "public.sdk/source/vst/hosting/plugprovider.cpp",
        "public.sdk/source/vst/hosting/processdata.cpp",
    ]
    .iter()
    .map(|p| sdk.join(p))
    .collect()
}

fn main() {
    let manifest = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").unwrap());
    let sdk = manifest.join("../../vendor/vst3sdk");
    let sdk = sdk.canonicalize().unwrap_or(sdk);
    let out = PathBuf::from(std::env::var("OUT_DIR").unwrap());
    assert!(
        sdk.join("public.sdk").is_dir(),
        "VST3 SDK submodule missing at {}; run `git submodule update --init --recursive vendor/vst3sdk`",
        sdk.display()
    );

    // --- Host static library: SDK hosting subset + our shim ----------------
    let mut host = cc::Build::new();
    host.cpp(true)
        .std("c++17")
        .include(&sdk)
        .warnings(false)
        .flag("-fPIC")
        .define("SMTG_OS_LINUX", "1")
        .define("RELEASE", "1");
    for group in [
        sdk_base(&sdk),
        sdk_pluginterfaces(&sdk),
        sdk_common(&sdk),
        sdk_vst(&sdk),
        sdk_hosting(&sdk),
    ] {
        host.files(&group);
    }
    host.file(manifest.join("src/shim.cpp"));
    host.compile("vibemix_vst3_sdk");

    println!("cargo:rustc-link-lib=dylib=stdc++");
    println!("cargo:rustc-link-lib=dylib=dl");
    println!("cargo:rustc-link-lib=dylib=pthread");
    println!("cargo:rustc-link-lib=dylib=m");
    // VST3 `IPlugView` hosting embeds the editor into a raw X11 window.
    println!("cargo:rustc-link-lib=dylib=X11");

    // --- Test effect, built as a .vst3 bundle ------------------------------
    let bundle = out.join("test.vst3");
    let arch_dir = bundle.join("Contents").join(format!("{}-linux", machine()));
    std::fs::create_dir_all(&arch_dir).expect("create bundle dirs");
    let so_path = arch_dir.join("test.so");

    let mut sources: Vec<PathBuf> = Vec::new();
    for group in [
        sdk_base(&sdk),
        sdk_pluginterfaces(&sdk),
        sdk_common(&sdk),
        sdk_vst(&sdk),
    ] {
        sources.extend(group);
    }
    sources.push(sdk.join("public.sdk/source/main/linuxmain.cpp"));
    sources.push(manifest.join("tests/test_effect.cpp"));

    let status = Command::new("c++")
        .arg("-shared")
        .arg("-fPIC")
        .arg("-std=c++17")
        .arg("-O2")
        .arg("-w")
        .arg("-DSMTG_OS_LINUX=1")
        .arg("-DRELEASE=1")
        .arg("-I")
        .arg(&sdk)
        .arg("-o")
        .arg(&so_path)
        .args(&sources)
        .arg("-ldl")
        .arg("-lpthread")
        .arg("-lm")
        .status()
        .expect("run c++ for the test effect");
    assert!(status.success(), "failed to build the test VST3 effect");

    println!("cargo:rustc-env=VIBEMIX_VST3_TEST_BUNDLE={}", bundle.display());
    println!("cargo:rerun-if-changed=src/shim.cpp");
    println!("cargo:rerun-if-changed=tests/test_effect.cpp");
}

fn machine() -> String {
    let out = Command::new("uname").arg("-m").output().expect("uname -m");
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}
