// build.rs — ncl-driver
//
// Two jobs:
//
// 1. When the `gui-app` feature is active (i.e. we are building the
//    packaged release binary), embed a Win32 application manifest
//    that declares:
//      • Per-Monitor DPI awareness (V2)
//      • Windows Vista / 7 / 8 / 8.1 / 10 / 11 compatibility
//      • asInvoker execution level (no UAC elevation prompt)
//      • UTF-8 active code page
//
// 2. Copy `LLVM-C.dll` from the LLVM install prefix next to the
//    built binaries so NCL runs without `<llvm>\bin` on PATH.  This
//    is the JASM-style trick that keeps the deployable folder
//    self-contained.  Reads `LLVM_SYS_221_PREFIX` (the env var
//    `llvm-sys` itself uses) and falls back to
//    `C:\Program Files\LLVM` if unset.
//
// On non-Windows hosts the embed-manifest crate is a no-op and the
// DLL-copy step is skipped via `cfg(target_os = "windows")`.

fn main() {
    println!("cargo:rerun-if-env-changed=LLVM_SYS_221_PREFIX");

    #[cfg(feature = "gui-app")]
    embed_manifest::embed_manifest(embed_manifest::new_manifest("NCL"))
        .expect("failed to embed application manifest");

    #[cfg(target_os = "windows")]
    copy_llvm_dll_next_to_binary();
}

#[cfg(target_os = "windows")]
fn copy_llvm_dll_next_to_binary() {
    use std::path::PathBuf;

    let llvm_prefix = std::env::var("LLVM_SYS_221_PREFIX")
        .unwrap_or_else(|_| r"C:\Program Files\LLVM".to_string());
    let dll = PathBuf::from(&llvm_prefix).join("bin").join("LLVM-C.dll");

    if !dll.exists() {
        // Soft-fail: cargo can still link if LLVM-C.lib is in lib/,
        // but the user will hit a missing-DLL error at runtime.  Print
        // a hint instead of panicking so dev cycles don't break on a
        // stale env var.
        println!(
            "cargo:warning=LLVM-C.dll not found at {} — set LLVM_SYS_221_PREFIX",
            dll.display()
        );
        return;
    }

    // OUT_DIR is `target/<profile>/build/<crate>-<hash>/out`.
    // Walk up 3 ancestors → `target/<profile>/` where the bin lands.
    let out_dir = PathBuf::from(std::env::var("OUT_DIR").unwrap());
    let Some(target_dir) = out_dir.ancestors().nth(3) else { return };

    let dest = target_dir.join("LLVM-C.dll");
    let needs_copy = match (std::fs::metadata(&dll), std::fs::metadata(&dest)) {
        (Ok(src_meta), Ok(dst_meta)) => src_meta.len() != dst_meta.len(),
        _ => true,
    };
    if needs_copy {
        if let Err(e) = std::fs::copy(&dll, &dest) {
            println!(
                "cargo:warning=copy LLVM-C.dll {} -> {} failed: {}",
                dll.display(), dest.display(), e
            );
            return;
        }
    }

    // Also drop next to deps/ and examples/ so doc tests and example
    // binaries can find it without extra wiring.
    for sub in &["deps", "examples"] {
        let d = target_dir.join(sub);
        if d.is_dir() {
            let _ = std::fs::copy(&dll, d.join("LLVM-C.dll"));
        }
    }
}
