//! Link settings for the macOS speech backend.
//!
//! An app that links this crate from another workspace needs the same rpath,
//! because a dependency's link arguments do not reach the final binary.

fn main() {
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("macos") {
        println!("cargo:rustc-link-arg=-Wl,-rpath,/usr/lib/swift");
    }
}
