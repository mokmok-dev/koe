//! `WebVTT` transcript formatter.

use std::fmt::Write as _;

use koe_ffi::TranscriptionSegment;

use super::{SegmentBuffer, TranscriptFormatter, format_timestamp};

/// `WebVTT` cues with recording-relative `HH:MM:SS.mmm` timestamps.
pub struct VttFormatter {
    buffer: SegmentBuffer,
}

impl VttFormatter {
    /// Creates an empty VTT formatter.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            buffer: SegmentBuffer::new(),
        }
    }

    fn render<'a>(
        segments: impl IntoIterator<Item = &'a TranscriptionSegment>,
    ) -> String {
        let mut out = String::from("WEBVTT\n");
        for (index, segment) in segments.into_iter().enumerate() {
            out.push('\n');
            let _ = writeln!(out, "{}", index + 1);
            let _ = writeln!(
                out,
                "{} --> {}",
                format_timestamp(segment.start_ms, '.'),
                format_timestamp(segment.end_ms, '.')
            );
            let _ = writeln!(out, "{}", segment.text);
        }
        out
    }
}

impl Default for VttFormatter {
    fn default() -> Self {
        Self::new()
    }
}

impl TranscriptFormatter for VttFormatter {
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
        start_ms: i64,
        end_ms: i64,
        is_final: bool,
    ) -> TranscriptionSegment {
        TranscriptionSegment {
            text: text.to_owned(),
            start_ms,
            end_ms,
            is_final,
            confidence: 0.95,
        }
    }

    #[test]
    fn vtt_matches_spec_example() {
        let mut fmt = VttFormatter::new();
        fmt.write_segment(&seg(
            "This is what was spoken in the first utterance.",
            1_250,
            4_800,
            true,
        ));
        fmt.write_segment(&seg(
            "This is the second utterance, which is longer.",
            5_100,
            9_200,
            true,
        ));
        let expected = "\
WEBVTT

1
00:00:01.250 --> 00:00:04.800
This is what was spoken in the first utterance.

2
00:00:05.100 --> 00:00:09.200
This is the second utterance, which is longer.
";
        assert_eq!(fmt.finalize(), expected);
    }

    #[test]
    fn vtt_empty_still_has_header() {
        let fmt = VttFormatter::new();
        assert_eq!(fmt.finalize(), "WEBVTT\n");
    }

    #[test]
    fn vtt_partial_in_preview_only() {
        let mut fmt = VttFormatter::new();
        fmt.write_segment(&seg("done", 0, 500, true));
        fmt.write_segment(&seg("draft", 500, 800, false));
        assert!(fmt.current_output().contains("draft"));
        assert!(!fmt.committed_output().contains("draft"));
        assert!(!fmt.finalize().contains("draft"));
    }
}
