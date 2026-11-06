#[cfg(feature = "rune-ui")]
fn main() {
    koe_desktop::run_rune();
}

#[cfg(all(not(feature = "rune-ui"), feature = "egui"))]
fn main() -> eframe::Result {
    koe_desktop::run()
}

#[cfg(not(any(feature = "rune-ui", feature = "egui")))]
fn main() {
    compile_error!("enable the rune-ui or egui feature");
}
