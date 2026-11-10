use std::{env, error::Error, fs, path::PathBuf, process::Command};

fn main() -> Result<(), Box<dyn Error>> {
    println!("cargo:rerun-if-changed=assets/icons");
    println!("cargo:rerun-if-changed=../../.github/release-notes");
    if env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("macos") {
        return Ok(());
    }
    let out = PathBuf::from(env::var_os("OUT_DIR").ok_or("Cargo provides OUT_DIR")?);
    let core = env::var_os("MOBIUS_CORE_DIR").map_or_else(|| PathBuf::from("../.."), PathBuf::from);
    for variable in ["MOBIUS_CORE_DIR", "DEVELOPER_DIR", "SDKROOT"] {
        println!("cargo:rerun-if-env-changed={variable}");
    }
    println!("cargo:rerun-if-changed=platform/Package.swift");
    println!("cargo:rerun-if-changed=platform/Package.resolved");
    for entry in fs::read_dir("platform")? {
        let path = entry?.path();
        if path.extension().is_some_and(|ext| ext == "swift") {
            println!("cargo:rerun-if-changed={}", path.display());
        }
    }
    println!("cargo:rerun-if-changed=platform/desktop");
    println!("cargo:rerun-if-changed=scripts/build-native.sh");
    println!("cargo:rerun-if-changed=scripts/swiftpm-cache.sh");
    println!(
        "cargo:rerun-if-changed={}/crates/mobius-gateway/src/wire.rs",
        core.display()
    );
    println!(
        "cargo:rerun-if-changed={}/.github/sync-gateway-contract.py",
        core.display()
    );
    let status = Command::new("bash")
        .args(["scripts/build-native.sh"])
        .arg(&out)
        .arg(&core)
        .status()?;
    if !status.success() {
        return Err(format!("native macOS build failed: {status}").into());
    }
    println!("cargo:rustc-link-search=native={}", out.display());
    println!("cargo:rustc-link-lib=dylib=MobiusNative");
    println!("cargo:rustc-link-arg=-Wl,-rpath,{}", out.display());
    println!("cargo:rustc-link-arg=-Wl,-rpath,@executable_path/../Frameworks");
    Ok(())
}
