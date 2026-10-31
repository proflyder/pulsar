//! Builds the Ricardo microWakeWord detector and its vendored microfrontend.
//!
//! The detector is C++ because it reuses the TensorFlow Lite microfrontend
//! sources and the firmware's TFLite C API; the daemon sees only the small C ABI
//! in `ricardo/microwakeword.cpp`. The model and libtflite.so stay on the
//! speaker, as before.

use std::env;
use std::path::{Path, PathBuf};

fn collect_cc(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            collect_cc(&path, out);
        } else if path.extension().is_some_and(|e| e == "cc") {
            out.push(path);
        }
    }
}

fn main() {
    let target = env::var("TARGET").unwrap_or_default();
    println!("cargo:rerun-if-changed=ricardo");
    println!("cargo:rerun-if-changed=build.rs");

    let mut sources = vec![PathBuf::from("ricardo/microwakeword.cpp")];
    collect_cc(Path::new("ricardo/microfrontend"), &mut sources);
    sources.sort();

    let mut build = cc::Build::new();
    build
        .cpp(true)
        .std("c++17")
        .include("ricardo/microfrontend")
        .include("ricardo/microfrontend/kissfft")
        .include("ricardo/microfrontend/kissfft/tools")
        .warnings(false);
    for source in sources {
        build.file(source);
    }
    build.compile("ricardo_mww");

    // The C++ runtime is linked by cc on the host. cargo-zigbuild's Linux target
    // uses zig's static libc++, as the sendspin shim does.
    if target.contains("linux") {
        println!("cargo:rustc-link-lib=dl");
        println!("cargo:rustc-link-lib=m");
        // cargo-zigbuild links zig's static libc++; a native Linux build uses
        // libstdc++ and must not ask for libc++.
        if env::var_os("CARGO_ZIGBUILD_ZIG_PATH").is_some() {
            println!("cargo:rustc-link-lib=c++");
        }
    }
}
