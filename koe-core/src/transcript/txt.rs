//! Plain-text transcript formatter.

use koe_ffi::TranscriptionSegment;

use super::TranscriptFormatter;

/// One line per finalized segment, no timestamps.
pub struct TxtFormatter {
    lines: Vec<String>,
}

impl TxtFormatter {
    /// Creates an empty TXT formatter.
    #[must_use]
    pub const fn new() -> Self {
        Self { lines: Vec::new() }
    }
}

impl Default for TxtFormatter {
    fn default() -> Self {
        Self::new()
    }
}

impl TranscriptFormatter for TxtFormatter {
    fn write_segment(
        &mut self,
        segment: &TranscriptionSegment,
    ) {
        if segment.is_final && !segment.text.is_empty() {
            self.lines.push(segment.text.clone());
        }
    }

    fn current_output(&self) -> String {
        self.lines.join("\n")
    }

    fn finalize(self) -> String {
        self.current_output()
    }
}
