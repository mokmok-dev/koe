//! Plain-text transcript formatter.

use koe_ffi::TranscriptionSegment;

use super::{SegmentBuffer, TranscriptFormatter};

/// One line per finalized segment, no timestamps.
pub struct TxtFormatter {
    buffer: SegmentBuffer,
}

impl TxtFormatter {
    /// Creates an empty TXT formatter.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            buffer: SegmentBuffer::new(),
        }
    }

    fn render<'a>(segments: impl IntoIterator<Item = &'a TranscriptionSegment>) -> String {
        let mut out = String::new();
        for (index, segment) in segments.into_iter().enumerate() {
            if index > 0 {
                out.push('\n');
            }
            out.push_str(&segment.text);
        }
        out
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
        self.buffer.write(segment);
    }

    fn current_output(&self) -> String {
        Self::render(
            self.buffer
                .finals
                .iter()
                .chain(self.buffer.partial.as_ref()),
        )
    }

    fn committed_output(&self) -> String {
        Self::render(&self.buffer.finals)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn seg(
        text: &str,
        is_final: bool,
    ) -> TranscriptionSegment {
        TranscriptionSegment {
            text: text.to_owned(),
            start_ms: 0,
            end_ms: 100,
            is_final,
            confidence: 0.9,
        }
    }

    #[test]
    fn txt_one_line_per_final_no_timestamps() {
        let mut fmt = TxtFormatter::new();
        fmt.write_segment(&seg("This is the first utterance.", true));
        fmt.write_segment(&seg("This is the second utterance.", true));
        assert_eq!(
            fmt.finalize(),
            "This is the first utterance.\nThis is the second utterance."
        );
    }

    #[test]
    fn txt_partial_in_current_output_excluded_from_finalize() {
        let mut fmt = TxtFormatter::new();
        fmt.write_segment(&seg("final line", true));
        fmt.write_segment(&seg("still talking", false));
        assert_eq!(fmt.current_output(), "final line\nstill talking");
        assert_eq!(fmt.committed_output(), "final line");
        assert_eq!(fmt.finalize(), "final line");
    }

    #[test]
    fn txt_skips_empty_text() {
        let mut fmt = TxtFormatter::new();
        fmt.write_segment(&seg("", true));
        fmt.write_segment(&seg("ok", true));
        assert_eq!(fmt.finalize(), "ok");
    }
}
