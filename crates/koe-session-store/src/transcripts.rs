//! The transcripts a meeting keeps, one per capture source.

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
}
