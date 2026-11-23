//! Finds the Blackmagic RAW SDK, generates the COM header from its IDL with MIDL and
//! compiles the C shim. Without the SDK, sets no `braw_sdk` cfg and the crate exposes
//! stubs that report "built without the Blackmagic RAW SDK".

use std::env;
use std::path::{Path, PathBuf};
use std::process::Command;

const DEFAULT_SDK: &str = r"C:\Program Files (x86)\Blackmagic Design\Blackmagic RAW\Blackmagic RAW SDK";

fn main() {
    println!("cargo::rustc-check-cfg=cfg(braw_sdk)");
    println!("cargo:rerun-if-env-changed=BRAW_SDK_DIR");
    println!("cargo:rerun-if-changed=shim/braw_shim.cpp");
    println!("cargo:rerun-if-changed=shim/braw_shim.h");

    let target = env::var("TARGET").unwrap();
    if !target.contains("windows-msvc") {
        println!("cargo:warning=braw-sys: only Windows/MSVC is wired up in v1; building without BRAW support");
        return;
    }

    let sdk = env::var("BRAW_SDK_DIR").map(PathBuf::from).unwrap_or_else(|_| PathBuf::from(DEFAULT_SDK));
    let inc = sdk.join("Win").join("Include");
    let idl = inc.join("BlackmagicRawAPI.idl");
    if !idl.exists() {
        println!(
            "cargo:warning=braw-sys: Blackmagic RAW SDK not found at {} (set BRAW_SDK_DIR). BRAW decoding disabled.",
            sdk.display()
        );
        return;
    }
    println!("cargo:rerun-if-changed={}", idl.display());

    let out = PathBuf::from(env::var("OUT_DIR").unwrap());
    let cl = cc::windows_registry::find_tool(&target, "cl.exe").expect("MSVC cl.exe not found");
    run_midl(&idl, &out, &cl);

    cc::Build::new()
        .cpp(true)
        .std("c++17")
        .include(&out)
        .include(&inc)
        .include("shim")
        .define("UNICODE", None)
        .define("_UNICODE", None)
        .define("NOMINMAX", None)
        .flag_if_supported("/EHsc")
        .file("shim/braw_shim.cpp")
        .file(inc.join("BlackmagicRawAPIDispatch.cpp"))
        .file(out.join("BlackmagicRawAPI_i.c"))
        .compile("braw_shim");

    println!("cargo:rustc-link-lib=ole32");
    println!("cargo:rustc-link-lib=oleaut32");
    println!("cargo:rustc-cfg=braw_sdk");
    println!("cargo:sdk_dir={}", sdk.display());
}

fn find_midl() -> PathBuf {
    let base = Path::new(r"C:\Program Files (x86)\Windows Kits\10\bin");
    let mut versions: Vec<PathBuf> = std::fs::read_dir(base)
        .map(|rd| rd.filter_map(|e| e.ok()).map(|e| e.path()).filter(|p| p.join("x64").join("midl.exe").exists()).collect())
        .unwrap_or_default();
    versions.sort();
    versions.pop().map(|v| v.join("x64").join("midl.exe")).expect("midl.exe not found in Windows Kits 10")
}

fn run_midl(idl: &Path, out: &Path, cl: &cc::Tool) {
    let midl = find_midl();
    let mut cmd = Command::new(&midl);
    // MIDL needs cl.exe (preprocessor) on PATH and the MSVC/SDK INCLUDE paths.
    for (k, v) in cl.get_envs() {
        cmd.env(k, v);
    }
    let cl_dir = cl.path().parent().unwrap().to_path_buf();
    let path = env::var_os("PATH").unwrap_or_default();
    let mut paths = vec![cl_dir];
    paths.extend(env::split_paths(&path));
    if let Some((_, p)) = cl.get_envs().find(|(k, _)| k.eq_ignore_ascii_case("PATH")) {
        paths.extend(env::split_paths(p));
    }
    cmd.env("PATH", env::join_paths(paths).unwrap());
    let status = cmd
        .current_dir(out)
        .args(["/nologo", "/env", "x64", "/h", "BlackmagicRawAPI.h", "/iid", "BlackmagicRawAPI_i.c", "/notlb", "/out"])
        .arg(out)
        .arg(idl)
        .status()
        .expect("failed to run midl.exe");
    assert!(status.success(), "midl.exe failed on {}", idl.display());
}
