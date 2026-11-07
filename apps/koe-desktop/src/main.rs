#[cfg(feature = "rune-ui")]
fn main() {
    koe_desktop::run_rune();
}

#[cfg(all(not(feature = "rune-ui"), feature = "gpui"))]
fn main() {
    koe_desktop::run_gpui();
}

#[cfg(not(any(feature = "rune-ui", feature = "gpui")))]
fn main() {
    compile_error!("enable the rune-ui or gpui feature");
}
