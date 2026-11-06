//! The wake-up callback shared by the UI, the recorder, and the tray.
//!
//! The desktop shell swaps the UI stack, so this module keeps the recorder and
//! the tray independent of eframe, egui, or Rune.

use std::sync::Arc;

/// Wakes the UI event loop. It must be safe to call from any thread.
pub type Repaint = Arc<dyn Fn() + Send + Sync>;

/// Wraps a callback as a [`Repaint`].
#[must_use]
pub fn repaint(f: impl Fn() + Send + Sync + 'static) -> Repaint {
    Arc::new(f)
}
