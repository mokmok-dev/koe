//! The koe desktop app: a menu bar recording control with an overlay HUD.
//!
//! The recorder and the tray are UI-independent. Build the Rune shell with the
//! default `rune-ui` feature, or the GPUI shell with `--no-default-features
//! --features gpui`.

pub mod dictation;
pub mod dictation_control;
pub mod dictation_worker;
pub mod engine;
pub mod error;
pub mod hotkey;
pub mod hud;
pub mod icon;
pub mod repaint;
pub mod settings;
pub mod state;
pub mod tray;

// GPUI macros emit `::gpui::` paths, which resolve through this alias when
// GPUI is consumed only through the Kit facade.
#[cfg(feature = "gpui")]
extern crate gpui_kit as gpui;

#[cfg(feature = "gpui")]
pub mod gpui_app;
#[cfg(feature = "gpui")]
pub mod gpui_hud;
#[cfg(feature = "gpui")]
pub mod gpui_ui;

#[cfg(feature = "rune-ui")]
pub mod rune_app;
#[cfg(feature = "rune-ui")]
pub mod rune_hud;
#[cfg(feature = "rune-ui")]
pub mod rune_ui;

/// Runs the GPUI shell.
#[cfg(feature = "gpui")]
pub fn run_gpui() {
    gpui_app::run();
}

/// Runs the Rune shell.
#[cfg(feature = "rune-ui")]
pub fn run_rune() {
    rune_app::run();
}
