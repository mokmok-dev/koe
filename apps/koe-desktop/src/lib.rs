//! The koe desktop app: a menu bar recording control with an overlay HUD.
//!
//! The recorder and the tray are UI-independent. Build the Rune shell with the
//! default `rune-ui` feature, or the previous egui shell with `--no-default-features
//! --features egui`.

pub mod engine;
pub mod error;
pub mod icon;
pub mod repaint;
pub mod settings;
pub mod state;
pub mod tray;

#[cfg(feature = "egui")]
pub mod app;
#[cfg(feature = "egui")]
pub mod hud;
#[cfg(feature = "egui")]
pub mod ui;

#[cfg(feature = "rune-ui")]
pub mod rune_app;
#[cfg(feature = "rune-ui")]
pub mod rune_hud;
#[cfg(feature = "rune-ui")]
pub mod rune_ui;

/// Runs the egui shell.
///
/// # Errors
///
/// Returns the eframe error when no graphics backend can be created.
#[cfg(feature = "egui")]
pub fn run() -> eframe::Result {
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("koe")
            .with_app_id("koe")
            .with_inner_size([440.0, 560.0])
            .with_min_inner_size([380.0, 460.0])
            .with_visible(false),
        ..Default::default()
    };
    eframe::run_native(
        "koe",
        options,
        Box::new(|cc| Ok(Box::new(app::KoeApp::new(cc)))),
    )
}

/// Runs the Rune shell.
#[cfg(feature = "rune-ui")]
pub fn run_rune() {
    rune_app::run();
}
