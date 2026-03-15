use std::env;
use std::path::PathBuf;
use std::process::Command;

fn main() {
    let out_dir = PathBuf::from(env::var("OUT_DIR").unwrap());
    let workspace_root = PathBuf::from(env::var("CARGO_MANIFEST_DIR").unwrap())
        .parent()
        .unwrap()
        .parent()
        .unwrap()
        .to_path_buf();

    let guest_manifest = workspace_root.join("crates/boa-wasm-guest/Cargo.toml");

    // Rebuild if the guest source changes
    println!(
        "cargo::rerun-if-changed={}",
        workspace_root.join("crates/boa-wasm-guest/src").display()
    );
    println!("cargo::rerun-if-changed={}", guest_manifest.display());

    // Build the guest crate for wasm32-wasip2
    let target_dir = out_dir.join("wasm-target");
    let status = Command::new("cargo")
        .args([
            "build",
            "--manifest-path",
            guest_manifest.to_str().unwrap(),
            "--target",
            "wasm32-wasip2",
            "--release",
            "--target-dir",
            target_dir.to_str().unwrap(),
        ])
        .status()
        .expect("failed to build boa-wasm-guest");

    if !status.success() {
        panic!("boa-wasm-guest build failed");
    }

    // Copy the wasm binary to OUT_DIR
    let wasm_src = target_dir.join("wasm32-wasip2/release/boa-wasm-guest.wasm");
    let wasm_dst = out_dir.join("boa_wasm_guest.wasm");
    std::fs::copy(&wasm_src, &wasm_dst).unwrap_or_else(|e| {
        panic!(
            "failed to copy {} -> {}: {e}",
            wasm_src.display(),
            wasm_dst.display()
        )
    });
}
