//! Transcript assembly.

use alloc::boxed::Box;
use alloc::string::String;
use alloc::vec::Vec;

use crate::error::Error;

/// One committed span of recognized speech.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Segment {
    start_ms: u64,
    end_ms: u64,
    text: Box<str>,
}

impl Segment {
    /// Builds a segment covering `start_ms`..`end_ms`.
    #[must_use]
    pub fn new(start_ms: u64, end_ms: u64, text: impl Into<Box<str>>) -> Self {
        Self {
            start_ms,
            end_ms,
            text: text.into(),
        }
    }

    /// Offset of the first sample the segment covers, in milliseconds.
    #[must_use]
    pub const fn start_ms(&self) -> u64 {
        self.start_ms
    }

    /// Offset one past the last sample the segment covers, in milliseconds.
    #[must_use]
    pub const fn end_ms(&self) -> u64 {
        self.end_ms
    }

    /// Recognized text.
    #[must_use]
    pub fn text(&self) -> &str {
        &self.text
    }
}

/// A finished transcription.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Transcript {
    text: Box<str>,
    segments: Box<[Segment]>,
    dropped_ms: u64,
}

impl Transcript {
    /// Builds a transcript from committed segments.
    ///
    /// [`text`](Self::text) is the segment text joined by single spaces.
    #[must_use]
    pub fn new(segments: Vec<Segment>, dropped_ms: u64) -> Self {
        let mut text = String::new();
        for (index, segment) in segments.iter().enumerate() {
            if index > 0 {
                text.push(' ');
            }
            text.push_str(&segment.text);
        }
        Self {
            text: text.into_boxed_str(),
            segments: segments.into_boxed_slice(),
            dropped_ms,
        }
    }

    /// Every segment's text, joined by a single space.
    #[must_use]
    pub fn text(&self) -> &str {
        &self.text
    }

    /// Committed segments in stream order.
    #[must_use]
    pub fn segments(&self) -> &[Segment] {
        &self.segments
    }

    /// Whether no text was recognized.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.text.is_empty()
    }

    /// Audio the session dropped instead of transcribing, in milliseconds.
    #[must_use]
    pub const fn dropped_ms(&self) -> u64 {
        self.dropped_ms
    }
}

/// A failure that still carries the transcript committed before it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Incomplete {
    error: Error,
    transcript: Transcript,
}

impl Incomplete {
    /// Pairs a failure with what was transcribed before it.
    #[must_use]
    pub const fn new(error: Error, transcript: Transcript) -> Self {
        Self { error, transcript }
    }

    /// What went wrong.
    #[must_use]
    pub const fn error(&self) -> &Error {
        &self.error
    }

    /// What was transcribed before the failure.
    #[must_use]
    pub const fn transcript(&self) -> &Transcript {
        &self.transcript
    }

    /// Keeps the transcript and discards the error.
    #[must_use]
    pub fn into_transcript(self) -> Transcript {
        self.transcript
    }
}

impl core::fmt::Display for Incomplete {
    fn fmt(&self, formatter: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(
            formatter,
            "{} ({} transcript bytes kept)",
            self.error,
            self.transcript.text().len()
        )
    }
}

impl core::error::Error for Incomplete {}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;

    fn segment(start_ms: u64, end_ms: u64, text: &str) -> Segment {
        Segment::new(start_ms, end_ms, text)
    }

    #[test]
    fn joins_segment_text_with_single_spaces() {
        let transcript = Transcript::new(
            vec![
                segment(0, 500, "hello"),
                segment(500, 900, "whole"),
                segment(900, 1_200, "world"),
            ],
            0,
        );
        assert_eq!(transcript.text(), "hello whole world");
        assert_eq!(transcript.segments().len(), 3);
        assert_eq!(transcript.segments()[1].start_ms(), 500);
        assert_eq!(transcript.segments()[1].end_ms(), 900);
        assert_eq!(transcript.segments()[1].text(), "whole");
        assert!(!transcript.is_empty());
    }

    #[test]
    fn an_empty_transcript_is_empty() {
        let transcript = Transcript::new(Vec::new(), 0);
        assert_eq!(transcript.text(), "");
        assert_eq!(transcript.segments().len(), 0);
        assert!(transcript.is_empty());
        assert_eq!(transcript, Transcript::default());
    }

    #[test]
    fn a_transcript_without_text_is_empty() {
        assert!(Transcript::new(vec![segment(0, 0, "")], 0).is_empty());
    }

    #[test]
    fn dropped_audio_is_reported() {
        assert_eq!(Transcript::new(Vec::new(), 250).dropped_ms(), 250);
    }

    #[test]
    fn incomplete_keeps_the_transcript_reachable() {
        let transcript = Transcript::new(vec![segment(0, 400, "kept")], 60);
        let incomplete = Incomplete::new(Error::Unsupported, transcript.clone());
        assert_eq!(incomplete.error(), &Error::Unsupported);
        assert_eq!(incomplete.transcript(), &transcript);
        assert_eq!(incomplete.into_transcript(), transcript);
    }

    #[test]
    fn incomplete_displays_the_error_and_the_kept_text_length() {
        let transcript = Transcript::new(vec![segment(0, 400, "kept")], 0);
        let incomplete = Incomplete::new(
            Error::Backend {
                operation: "run",
                code: -3,
            },
            transcript,
        );
        assert_eq!(
            incomplete.to_string(),
            "speech backend `run` failed with code -3 (4 transcript bytes kept)"
        );
    }
}
