//! The transcripts a meeting keeps, one per capture source.

use std::fmt::Write as _;

use koe_capture_core::Source;
use koe_transcribe_core::Transcript;

/// A meeting's finished transcripts.
///
/// A source is absent when the meeting never produced a transcript for it,
/// which is not the same as holding an empty transcript.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Transcripts {
    microphone: Option<Transcript>,
    system_audio: Option<Transcript>,
}

impl Transcripts {
    /// No transcript for either source.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            microphone: None,
            system_audio: None,
        }
    }

    /// The transcript `source` holds, if any.
    #[must_use]
    pub const fn get(&self, source: Source) -> Option<&Transcript> {
        match source {
            Source::Microphone => self.microphone.as_ref(),
            Source::SystemAudio => self.system_audio.as_ref(),
        }
    }

    /// Replaces `source`'s transcript, or drops it when `transcript` is [`None`].
    pub fn set(&mut self, source: Source, transcript: Option<Transcript>) {
        match source {
            Source::Microphone => self.microphone = transcript,
            Source::SystemAudio => self.system_audio = transcript,
        }
    }

    /// Whether neither source holds a transcript.
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.microphone.is_none() && self.system_audio.is_none()
    }

    /// Renders both sources as one `[start..end] source: text` line per
    /// segment, merged in time order.
    ///
    /// Segments that start at the same millisecond keep the microphone first
    /// and then their stream order, so the same transcripts always render to
    /// the same text.
    #[must_use]
    pub(crate) fn render(&self) -> String {
        let mut segments = Vec::new();
        for source in [Source::Microphone, Source::SystemAudio] {
            if let Some(transcript) = self.get(source) {
                segments.extend(
                    transcript
                        .segments()
                        .iter()
                        .map(|segment| (source, segment)),
                );
            }
        }
        segments.sort_by_key(|(source, segment)| (segment.start_ms(), *source));
        let mut text = String::new();
        for (source, segment) in segments {
            let _ = writeln!(
                text,
                "[{}..{}] {source}: {}",
                segment.start_ms(),
                segment.end_ms(),
                segment.text()
            );
        }
        text
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use koe_transcribe_core::Segment;

    use super::*;

    fn transcript(segments: Vec<Segment>) -> Transcript {
        Transcript::new(segments, 0)
    }

    #[test]
    fn an_empty_set_renders_nothing() {
        assert_eq!(Transcripts::new().render(), "");
        let mut transcripts = Transcripts::new();
        transcripts.set(Source::Microphone, Some(transcript(Vec::new())));
        assert_eq!(transcripts.render(), "");
    }

    #[test]
    fn both_sources_merge_in_time_order() {
        let mut transcripts = Transcripts::new();
        transcripts.set(
            Source::Microphone,
            Some(transcript(vec![
                Segment::new(0, 500, "hello"),
                Segment::new(900, 1_200, "again"),
            ])),
        );
        transcripts.set(
            Source::SystemAudio,
            Some(transcript(vec![Segment::new(600, 800, "remote")])),
        );
        assert_eq!(
            transcripts.render(),
            "[0..500] microphone: hello\n\
             [600..800] system audio: remote\n\
             [900..1200] microphone: again\n"
        );
    }

    #[test]
    fn a_tie_keeps_the_microphone_first_and_stream_order_within_a_source() {
        let mut transcripts = Transcripts::new();
        transcripts.set(
            Source::Microphone,
            Some(transcript(vec![
                Segment::new(500, 600, "first"),
                Segment::new(500, 700, "second"),
            ])),
        );
        transcripts.set(
            Source::SystemAudio,
            Some(transcript(vec![Segment::new(500, 550, "remote")])),
        );
        assert_eq!(
            transcripts.render(),
            "[500..600] microphone: first\n\
             [500..700] microphone: second\n\
             [500..550] system audio: remote\n"
        );
    }

    #[test]
    fn a_missing_source_does_not_shift_the_other() {
        let mut transcripts = Transcripts::new();
        transcripts.set(
            Source::SystemAudio,
            Some(transcript(vec![Segment::new(0, 100, "only")])),
        );
        assert_eq!(transcripts.render(), "[0..100] system audio: only\n");
    }
}
