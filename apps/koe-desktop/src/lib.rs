//! The koe desktop app: a menu bar recording control with an overlay HUD.
//!
//! The recorder and the tray are UI-independent.

pub mod app;
pub mod engine;
pub mod error;
pub mod hud;
pub mod icon;
pub mod overlay;
pub mod settings;
pub mod state;
pub mod tray;
pub mod ui;

// GPUI macros emit `::gpui::` paths, which resolve through this alias when
// GPUI is consumed only through the Kit facade.
extern crate gpui_kit as gpui;
