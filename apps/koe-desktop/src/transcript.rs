//! The meeting transcript the window keeps.

use std::time::{Duration, SystemTime};

use chrono::{DateTime, Local};
use gpui::SharedString;
use koe_capture::Source;

/// Who a meeting utterance belongs to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Speaker {
    /// The microphone, which carries the local voice.
    You,
    /// The system audio, which carries the remote voices.
    Them,
}

impl Speaker {
    /// The speaker a capture source belongs to.
    #[must_use]
    pub const fn of(source: Source) -> Self {
        match source {
            Source::Microphone => Self::You,
            Source::SystemAudio => Self::Them,
        }
    }

    /// The label the transcript shows.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::You => "You",
            Self::Them => "Them",
        }
    }

    /// Index of the speaker's slot in [`MeetingTranscript::drafts`].
    pub(crate) const fn index(self) -> usize {
        match self {
            Self::You => 0,
            Self::Them => 1,
        }
    }
}

/// One finalized utterance.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Utterance {
    /// Identity that survives insertion and a later take.
    pub sequence: u64,
    /// Who spoke.
    pub speaker: Speaker,
    /// Time the utterance began.
    pub at: SystemTime,
    /// `at` as local `HH:MM:SS`, formatted once.
    pub clock: SharedString,
    /// Recognized text, trimmed.
    pub text: SharedString,
}

/// Text the engine is still revising.
///
/// A draft is not part of the retained transcript: the next commit for its
/// speaker replaces it, and it is never written out.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Draft {
    /// Who is speaking.
    pub speaker: Speaker,
    /// When the draft arrived.
    pub at: SystemTime,
    /// `at` as local `HH:MM:SS`, formatted once.
    pub clock: SharedString,
    /// Text so far, trimmed.
    pub text: SharedString,
}

/// The utterances of a meeting, oldest first.
///
/// Utterances with the same time keep their arrival order.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct MeetingTranscript {
    utterances: Vec<Utterance>,
    /// At most one draft per speaker, indexed by [`Speaker::index`].
    drafts: [Option<Draft>; 2],
    next_sequence: u64,
}

impl MeetingTranscript {
    /// Appends an utterance in time order, dropping one whose text is only
    /// whitespace.
    ///
    /// The speaker's draft is cleared: the commit is what the draft became.
    pub fn push(&mut self, speaker: Speaker, at: SystemTime, text: &str) {
        let text = text.trim();
        if text.is_empty() {
            return;
        }
        self.drafts[speaker.index()] = None;
        let index = self
            .utterances
            .partition_point(|utterance| utterance.at <= at);
        self.utterances.insert(
            index,
            Utterance {
                sequence: self.next_sequence,
                speaker,
                at,
                clock: SharedString::from(clock_time(at)),
                text: SharedString::from(text),
            },
        );
        self.next_sequence += 1;
    }

    /// Sets the speaker's provisional text, or clears it when `text` is only
    /// whitespace.
    pub fn draft(&mut self, speaker: Speaker, at: SystemTime, text: &str) {
        let text = text.trim();
        self.drafts[speaker.index()] = (!text.is_empty()).then(|| Draft {
            speaker,
            at,
            clock: SharedString::from(clock_time(at)),
            text: SharedString::from(text),
        });
    }

    /// Starts a new take's transcript with `text`, or leaves the current
    /// transcript when `text` is only whitespace.
    ///
    /// Returns whether the current transcript was replaced.
    pub fn replace(&mut self, speaker: Speaker, at: SystemTime, text: &str) -> bool {
        let mut candidate = Self {
            utterances: Vec::new(),
            drafts: [None, None],
            next_sequence: self.next_sequence,
        };
        candidate.push(speaker, at, text);
        if candidate.is_empty() {
            return false;
        }
        *self = candidate;
        true
    }

    /// The retained utterances, oldest first.
    #[must_use]
    pub fn utterances(&self) -> &[Utterance] {
        &self.utterances
    }

    /// The provisional text of each speaker that has some, below the thread.
    pub fn drafts(&self) -> impl Iterator<Item = &Draft> {
        self.drafts.iter().flatten()
    }

    /// Drops every provisional draft, keeping the retained transcript.
    pub fn clear_drafts(&mut self) {
        self.drafts = [None, None];
    }

    /// Whether no text is retained and none is in progress.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.utterances.is_empty() && self.drafts.iter().all(Option::is_none)
    }

    /// Number of retained utterances.
    #[must_use]
    pub const fn len(&self) -> usize {
        self.utterances.len()
    }
}

/// Shown when a take starts without a speech model, so no transcript follows.
pub const MODEL_MISSING: &str = "No transcription model is set: this take is recorded without a transcript. Set one in Settings → Dictation → Model.";

/// The wall-clock time `offset` after `start`.
#[must_use]
pub(crate) fn at_offset(start: SystemTime, offset: Duration) -> SystemTime {
    start.checked_add(offset).unwrap_or(start)
}

/// `at` as local wall-clock time, `HH:MM:SS`.
fn clock_time(at: SystemTime) -> String {
    let local = DateTime::<Local>::from(at);
    local.format("%H:%M:%S").to_string()
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use std::time::UNIX_EPOCH;

    use super::*;

    fn at(seconds: u64) -> SystemTime {
        UNIX_EPOCH + Duration::from_secs(seconds)
    }

    #[test]
    fn the_microphone_is_you_and_system_audio_is_them() {
        assert_eq!(Speaker::of(Source::Microphone), Speaker::You);
        assert_eq!(Speaker::of(Source::SystemAudio), Speaker::Them);
        assert_eq!(Speaker::You.label(), "You");
        assert_eq!(Speaker::Them.label(), "Them");
    }

    #[test]
    fn utterances_are_kept_in_time_order() {
        let mut transcript = MeetingTranscript::default();
        transcript.push(Speaker::Them, at(200), "later");
        transcript.push(Speaker::You, at(100), "earlier");
        assert_eq!(
            transcript.utterances(),
            [
                Utterance {
                    sequence: 1,
                    speaker: Speaker::You,
                    at: at(100),
                    clock: SharedString::from(clock_time(at(100))),
                    text: SharedString::from("earlier"),
                },
                Utterance {
                    sequence: 0,
                    speaker: Speaker::Them,
                    at: at(200),
                    clock: SharedString::from(clock_time(at(200))),
                    text: SharedString::from("later"),
                },
            ]
        );
        assert_eq!(transcript.len(), 2);
        assert!(!transcript.is_empty());
    }

    #[test]
    fn equal_times_keep_arrival_order() {
        let mut transcript = MeetingTranscript::default();
        transcript.push(Speaker::You, at(100), "first");
        transcript.push(Speaker::Them, at(100), "second");
        assert_eq!(transcript.utterances()[0].text, "first");
        assert_eq!(transcript.utterances()[1].text, "second");
    }

    #[test]
    fn whitespace_text_is_not_retained() {
        let mut transcript = MeetingTranscript::default();
        transcript.push(Speaker::You, at(0), "   ");
        assert!(transcript.is_empty());
        assert_eq!(transcript.len(), 0);
    }

    #[test]
    fn a_draft_is_text_in_progress_not_a_retained_utterance() {
        let mut transcript = MeetingTranscript::default();
        transcript.draft(Speaker::You, at(10), "  hello ");
        assert_eq!(transcript.len(), 0);
        assert!(!transcript.is_empty(), "the draft is text to show");
        let draft = transcript.drafts().next().unwrap();
        assert_eq!(draft.text, "hello");
        assert_eq!(draft.speaker, Speaker::You);
    }

    #[test]
    fn a_commit_clears_its_speakers_draft_and_keeps_the_other() {
        let mut transcript = MeetingTranscript::default();
        transcript.draft(Speaker::You, at(10), "mine");
        transcript.draft(Speaker::Them, at(11), "theirs");
        transcript.push(Speaker::You, at(10), "mine final");
        assert_eq!(transcript.len(), 1);
        let drafts: Vec<_> = transcript.drafts().collect();
        assert_eq!(drafts.len(), 1);
        assert_eq!(drafts[0].speaker, Speaker::Them);
    }

    #[test]
    fn an_empty_draft_clears_the_speakers_text() {
        let mut transcript = MeetingTranscript::default();
        transcript.draft(Speaker::You, at(10), "hello");
        transcript.draft(Speaker::You, at(12), "   ");
        assert_eq!(transcript.drafts().count(), 0);
        assert!(transcript.is_empty());
    }

    #[test]
    fn a_new_take_drops_the_previous_drafts() {
        let mut transcript = MeetingTranscript::default();
        transcript.draft(Speaker::Them, at(10), "theirs");
        transcript.clear_drafts();
        assert_eq!(transcript.drafts().count(), 0);
    }

    #[test]
    fn a_replacement_keeps_row_identity_monotonic() {
        let mut transcript = MeetingTranscript::default();
        transcript.push(Speaker::You, at(10), "old");
        let old = transcript.utterances()[0].sequence;
        assert!(transcript.replace(Speaker::Them, at(20), "new"));
        assert_eq!(transcript.len(), 1);
        assert!(transcript.utterances()[0].sequence > old);
    }

    #[test]
    fn a_whitespace_replacement_keeps_the_old_transcript() {
        let mut transcript = MeetingTranscript::default();
        transcript.push(Speaker::You, at(10), "old");
        assert!(!transcript.replace(Speaker::Them, at(20), "   "));
        assert_eq!(transcript.utterances()[0].text, "old");
    }

    #[test]
    fn a_time_formats_as_local_hours_minutes_and_seconds() {
        let at = at(1_760_000_000);
        let expected = DateTime::<Local>::from(at).format("%H:%M:%S").to_string();
        assert_eq!(clock_time(at), expected);
        assert_eq!(expected.len(), 8);
        assert_eq!(
            (expected.as_bytes()[2], expected.as_bytes()[5]),
            (b':', b':')
        );
    }

    #[test]
    fn an_offset_moves_the_clock_forward() {
        let start = at(0);
        assert_eq!(
            at_offset(start, Duration::from_millis(90_500)),
            start + Duration::from_millis(90_500)
        );
    }
}
