// Only relevant for the `x86_64-pc-windows-gnullvm` target built WITHOUT llvm-mingw:
// rustup's bundled self-contained libs lack most Windows import libraries
// (advapi32, gdi32, ...). The `winapi-x86_64-pc-windows-gnu` crate ships the same
// libraries as `libwinapi_<name>.a`; we copy them as `lib<name>.a` into OUT_DIR and
// add it to the link search path. On MSVC (the default toolchain) this is a no-op.
use std::{env, fs, path::PathBuf};

const LIBS: &[&str] = &[
    "advapi32", "cfgmgr32", "credui", "gdi32", "msimg32", "opengl32", "secur32", "synchronization", "winspool",
    "ole32", "oleaut32", "shell32", "uuid", "winmm", "bcrypt", "crypt32", "ncrypt", "iphlpapi", "psapi",
    "shlwapi", "version", "comdlg32", "comctl32", "imm32", "setupapi", "avrt", "ksuser", "mfplat", "mf", "mfuuid",
];

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    if env::var("TARGET").map(|t| t != "x86_64-pc-windows-gnullvm").unwrap_or(true) {
        return;
    }
    let home = env::var("CARGO_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from(env::var("USERPROFILE").expect("USERPROFILE")).join(".cargo"));
    let src = fs::read_dir(home.join("registry").join("src"))
        .expect("cargo registry/src")
        .flatten()
        .map(|e| e.path().join("winapi-x86_64-pc-windows-gnu-0.4.0").join("lib"))
        .find(|p| p.is_dir())
        .expect("winapi-x86_64-pc-windows-gnu-0.4.0 not found in the cargo registry (run `cargo fetch`)");
    let out = PathBuf::from(env::var("OUT_DIR").unwrap()).join("mingw-libs");
    fs::create_dir_all(&out).unwrap();
    for lib in LIBS {
        let from = src.join(format!("libwinapi_{lib}.a"));
        if from.exists() {
            fs::copy(&from, out.join(format!("lib{lib}.a"))).unwrap();
        }
    }
    println!("cargo:rustc-link-search=native={}", out.display());
}
