//! Transcript formatting (full SRT/VTT/JSON in task 20).

mod txt;

use koe_ffi::{TranscriptFormat, TranscriptionSegment};

pub use txt::TxtFormatter;

/// Formats finalized transcription segments for file output.
pub trait TranscriptFormatter: Send {
    /// Records a finalized segment.
    fn write_segment(
        &mut self,
        segment: &TranscriptionSegment,
    );

    /// Returns the in-progress transcript (for live preview).
    fn current_output(&self) -> String;

    /// Returns the complete transcript after recording stops.
    fn finalize(self) -> String;
}

/// Creates a formatter for the requested transcript format.
#[must_use]
pub fn create_formatter(format: TranscriptFormat) -> Box<dyn TranscriptFormatter> {
    match format {
        TranscriptFormat::Txt => Box::new(TxtFormatter::new()),
        TranscriptFormat::Srt | TranscriptFormat::Vtt | TranscriptFormat::Json => {
            Box::new(TxtFormatter::new())
        },
    }
}
