//! Compiles the Swift shim when the target runs macOS.

use std::env;
use std::ffi::OsStr;
use std::path::Path;
use std::process::Command;

fn main() {
    println!("cargo:rerun-if-changed=shim/SpeechShim.swift");
    println!("cargo:rerun-if-changed=shim/include/koe_speech.h");
    if env::var_os("CARGO_CFG_TARGET_OS").as_deref() != Some(OsStr::new("macos")) {
        return;
    }
    let Some(arch) = swift_arch() else {
        println!(
            "cargo:warning=koe-transcribe-macos: target arch has no Swift target; skipping koe_speech_shim"
        );
        return;
    };
    let Some(swiftc) = find_swiftc() else {
        println!(
            "cargo:warning=koe-transcribe-macos: swiftc not found; koe_speech_shim will not be built, so macOS transcription cannot link"
        );
        return;
    };
    let Ok(out_dir) = env::var("OUT_DIR") else {
        println!("cargo:warning=koe-transcribe-macos: OUT_DIR unset; skipping koe_speech_shim");
        return;
    };
    let Ok(manifest_dir) = env::var("CARGO_MANIFEST_DIR") else {
        println!(
            "cargo:warning=koe-transcribe-macos: CARGO_MANIFEST_DIR unset; skipping koe_speech_shim"
        );
        return;
    };
    let mut command = Command::new(&swiftc);
    command
        .args(["-emit-library", "-static", "-swift-version", "5"])
        .args(["-target", &format!("{arch}-apple-macos14.0")]);
    if let Some(sdk) = sdk_path() {
        command.args(["-sdk", &sdk]);
    }
    let header = Path::new(&manifest_dir).join("shim/include/koe_speech.h");
    let source = Path::new(&manifest_dir).join("shim/SpeechShim.swift");
    let library = Path::new(&out_dir).join("libkoe_speech_shim.a");
    command
        .arg("-import-objc-header")
        .arg(&header)
        .arg("-o")
        .arg(&library)
        .arg(&source);
    match command.status() {
        Ok(status) if status.success() => {}
        Ok(status) => {
            eprintln!("swiftc exited with {status}: koe_speech_shim build failed");
            std::process::exit(1);
        }
        Err(error) => {
            eprintln!("could not run {swiftc}: {error}");
            std::process::exit(1);
        }
    }
    println!("cargo:rustc-link-search=native={out_dir}");
    println!("cargo:rustc-link-lib=static=koe_speech_shim");
    // The archive auto-links the Swift runtime, which lives beside the
    // compiler rather than in the default search path.
    for path in runtime_library_paths(&swiftc) {
        println!("cargo:rustc-link-search=native={path}");
    }
    // Weak frameworks keep binaries loadable on macOS versions below 26.
    println!("cargo:rustc-link-arg=-Wl,-weak_framework,Speech");
    println!("cargo:rustc-link-arg=-Wl,-weak_framework,AVFAudio");
    // The archive references the system Swift runtime through `@rpath`.
    println!("cargo:rustc-link-arg=-Wl,-rpath,/usr/lib/swift");
}

/// Directories holding the Swift runtime libraries, as the compiler reports
/// them.
fn runtime_library_paths(swiftc: &str) -> Vec<String> {
    let Ok(output) = Command::new(swiftc).arg("-print-target-info").output() else {
        return Vec::new();
    };
    if !output.status.success() {
        return Vec::new();
    }
    let info = String::from_utf8_lossy(&output.stdout);
    let Some(key) = info.find("\"runtimeLibraryPaths\"") else {
        return Vec::new();
    };
    let Some(open) = info[key..].find('[').map(|offset| key + offset) else {
        return Vec::new();
    };
    let Some(close) = info[open..].find(']').map(|offset| open + offset) else {
        return Vec::new();
    };
    info[open + 1..close]
        .split('"')
        .skip(1)
        .step_by(2)
        .filter(|path| !path.is_empty())
        .map(str::to_owned)
        .collect()
}

/// Swift's target spelling for the configured Cargo target architecture.
fn swift_arch() -> Option<String> {
    match env::var("CARGO_CFG_TARGET_ARCH").as_deref() {
        Ok("aarch64") => Some(String::from("arm64")),
        Ok("x86_64") => Some(String::from("x86_64")),
        _ => None,
    }
}

/// Locates `swiftc`, preferring the toolchain on PATH, then `xcrun --find`.
fn find_swiftc() -> Option<String> {
    if Command::new("swiftc").arg("--version").output().is_ok() {
        return Some(String::from("swiftc"));
    }
    let output = Command::new("xcrun")
        .args(["--find", "swiftc"])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let path = String::from_utf8(output.stdout).ok()?;
    let path = path.trim();
    (!path.is_empty()).then_some(String::from(path))
}

/// macOS SDK the shim compiles against.
fn sdk_path() -> Option<String> {
    let output = Command::new("xcrun").arg("--show-sdk-path").output().ok()?;
    if !output.status.success() {
        return None;
    }
    let path = String::from_utf8(output.stdout).ok()?;
    let path = path.trim();
    (!path.is_empty()).then_some(String::from(path))
}
