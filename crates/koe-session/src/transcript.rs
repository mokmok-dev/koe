//! Transcript progress and the faults that make it incomplete.

use core::fmt;

use koe_capture_core::Source;

use crate::sources::index;

/// What went wrong with one source's transcription.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Fault {
    /// The capture stream failed.
    Capture(Source),
    /// The capture device could not be opened or was lost.
    Device(Source),
    /// The transcription engine failed or lost audio.
    Engine(Source),
    /// The permission was revoked mid-recording.
    Permission(Source),
}

impl fmt::Display for Fault {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let (what, source) = match self {
            Self::Capture(source) => ("capture failed", source),
            Self::Device(source) => ("device failed", source),
            Self::Engine(source) => ("engine failed", source),
            Self::Permission(source) => ("permission revoked", source),
        };
        write!(formatter, "{what} for {source}")
    }
}

/// Whether a session produced a complete transcript.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Outcome {
    /// Everything recorded was transcribed.
    Complete,
    /// Something was lost, and the first loss stays reported.
    Incomplete(Fault),
}

impl Outcome {
    /// Whether a fault was recorded.
    #[must_use]
    pub const fn is_incomplete(self) -> bool {
        matches!(self, Self::Incomplete(_))
    }

    /// The first fault recorded, if any.
    #[must_use]
    pub const fn fault(self) -> Option<Fault> {
        match self {
            Self::Complete => None,
            Self::Incomplete(fault) => Some(fault),
        }
    }

    /// Keeps the first fault when a second arrives.
    pub(crate) const fn or_incomplete(self, fault: Fault) -> Self {
        match self {
            Self::Complete => Self::Incomplete(fault),
            Self::Incomplete(_) => self,
        }
    }
}

/// What one closed transcription segment reported.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum SegmentOutcome {
    /// The segment transcribed everything it covered.
    Complete,
    /// The segment lost audio.
    Incomplete,
}

/// The result a transcription session reports when its last segment closes.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct SegmentResult {
    /// Whether the segment transcribed everything it covered.
    pub outcome: SegmentOutcome,
    /// Whether the segment committed text.
    pub committed: bool,
}

/// Which sources have text, and which still hold text that a later segment can
/// replace.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct Transcript {
    committed: [bool; 2],
    volatile: [bool; 2],
}

impl Transcript {
    pub(crate) const fn new() -> Self {
        Self {
            committed: [false; 2],
            volatile: [false; 2],
        }
    }

    /// Whether `source` has committed text.
    #[must_use]
    pub const fn committed(self, source: Source) -> bool {
        self.committed[index(source)]
    }

    /// Whether `source` holds text a later segment can still replace.
    #[must_use]
    pub const fn volatile(self, source: Source) -> bool {
        self.volatile[index(source)]
    }

    /// Whether any source has committed text.
    #[must_use]
    pub const fn any_committed(self) -> bool {
        self.committed[0] || self.committed[1]
    }

    /// Whether any source holds replaceable text.
    #[must_use]
    pub const fn any_volatile(self) -> bool {
        self.volatile[0] || self.volatile[1]
    }

    pub(crate) const fn set_committed(&mut self, source: Source, committed: bool) {
        self.committed[index(source)] = committed;
    }

    pub(crate) const fn set_volatile(&mut self, source: Source, volatile: bool) {
        self.volatile[index(source)] = volatile;
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;

    #[test]
    fn default_transcript_has_nothing() {
        let transcript = Transcript::default();
        assert!(!transcript.committed(Source::Microphone));
        assert!(!transcript.committed(Source::SystemAudio));
        assert!(!transcript.volatile(Source::Microphone));
        assert!(!transcript.volatile(Source::SystemAudio));
        assert!(!transcript.any_committed());
        assert!(!transcript.any_volatile());
        assert_eq!(Transcript::new(), transcript);
    }

    #[test]
    fn text_is_tracked_per_source() {
        let mut transcript = Transcript::new();
        transcript.set_committed(Source::SystemAudio, true);
        assert!(!transcript.committed(Source::Microphone));
        assert!(transcript.committed(Source::SystemAudio));
        assert!(transcript.any_committed());

        transcript.set_volatile(Source::Microphone, true);
        assert!(transcript.volatile(Source::Microphone));
        assert!(!transcript.volatile(Source::SystemAudio));
        assert!(transcript.any_volatile());

        transcript.set_volatile(Source::Microphone, false);
        assert!(!transcript.volatile(Source::Microphone));
        assert!(!transcript.any_volatile());

        transcript.set_committed(Source::SystemAudio, false);
        assert!(!transcript.any_committed());
    }

    #[test]
    fn the_first_fault_is_kept() {
        let complete = Outcome::Complete;
        assert!(!complete.is_incomplete());
        assert_eq!(complete.fault(), None);

        let capture = complete.or_incomplete(Fault::Capture(Source::Microphone));
        assert!(capture.is_incomplete());
        assert_eq!(capture.fault(), Some(Fault::Capture(Source::Microphone)));

        let engine = capture.or_incomplete(Fault::Engine(Source::SystemAudio));
        assert_eq!(engine, capture);
        assert_eq!(engine.fault(), Some(Fault::Capture(Source::Microphone)));
    }

    #[test]
    fn faults_name_the_source() {
        assert_eq!(
            Fault::Capture(Source::Microphone).to_string(),
            "capture failed for microphone"
        );
        assert_eq!(
            Fault::Device(Source::SystemAudio).to_string(),
            "device failed for system audio"
        );
        assert_eq!(
            Fault::Engine(Source::Microphone).to_string(),
            "engine failed for microphone"
        );
        assert_eq!(
            Fault::Permission(Source::SystemAudio).to_string(),
            "permission revoked for system audio"
        );
    }

    #[test]
    fn a_segment_result_carries_both_axes() {
        let result = SegmentResult {
            outcome: SegmentOutcome::Incomplete,
            committed: true,
        };
        assert_eq!(result.outcome, SegmentOutcome::Incomplete);
        assert!(result.committed);
    }
}
