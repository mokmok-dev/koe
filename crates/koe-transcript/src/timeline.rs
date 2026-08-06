//! Timeline accessors over finalized segments for export and display.

use super::types::TranscriptSegment;

/// Sorted transcript snapshot.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct TimelineSnapshot {
    segments: Vec<TranscriptSegment>,
}

impl TimelineSnapshot {
    /// Builds a snapshot sorted by start time (stable per segment id).
    #[must_use]
    pub fn new(mut segments: Vec<TranscriptSegment>) -> Self {
        segments.sort_by_key(|segment| (segment.start_ms, segment.segment_id));
        Self { segments }
    }

    /// Segments in timeline order.
    #[must_use]
    pub fn segments(&self) -> &[TranscriptSegment] {
        &self.segments
    }

    /// Segment containing `time_ms`, if any.
    #[must_use]
    pub fn segment_at(
        &self,
        time_ms: u64,
    ) -> Option<&TranscriptSegment> {
        self.segments
            .iter()
            .find(|segment| time_ms >= segment.start_ms && time_ms < segment.end_ms)
    }

    /// Whether the timeline has any segment.
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.segments.is_empty()
    }

    /// End of the last segment in milliseconds.
    #[must_use]
    pub fn duration_ms(&self) -> u64 {
        self.segments
            .iter()
            .map(|segment| segment.end_ms)
            .max()
            .unwrap_or(0)
    }
}

/// Renders the transcript as a text block with `[mm:ss.mmm]` markers.
#[must_use]
pub fn format_plain_text(segments: &[TranscriptSegment]) -> String {
    use std::fmt::Write as _;

    let mut output = String::new();
    for segment in segments {
        let _ignored = writeln!(
            output,
            "[{}] {}",
            format_clock(segment.start_ms),
            segment.text
        );
    }
    output
}

/// Formats milliseconds as `mm:ss.mmm`.
#[must_use]
pub fn format_clock(ms: u64) -> String {
    let minutes = ms / 60_000;
    let seconds = (ms % 60_000) / 1_000;
    let millis = ms % 1_000;
    format!("{minutes:02}:{seconds:02}.{millis:03}")
}

#[cfg(test)]
mod tests {
    use super::{TimelineSnapshot, format_clock, format_plain_text};
    use crate::{SegmentId, TranscriptSegment};

    #[allow(clippy::needless_pass_by_value)]
    fn segment(
        _sequence: u64,
        start_ms: u64,
        end_ms: u64,
        text: &str,
    ) -> TranscriptSegment {
        TranscriptSegment {
            schema_version: 1,
            segment_id: SegmentId::new(),
            source: "mixed".to_owned(),
            start_ms,
            end_ms,
            text: text.to_owned(),
            is_final: true,
            model: None,
            audio_discontinuities: Vec::new(),
        }
    }

    #[test]
    fn snapshot_sorts_by_start_time() {
        let snapshot = TimelineSnapshot::new(vec![
            segment(3, 3000, 4000, "c"),
            segment(1, 1000, 2000, "a"),
            segment(2, 2000, 3000, "b"),
        ]);
        assert_eq!(
            snapshot
                .segments()
                .iter()
                .map(|segment| segment.text.as_str())
                .collect::<Vec<_>>(),
            vec!["a", "b", "c"]
        );
    }

    #[test]
    fn segment_at_answers_timeline_lookup() {
        let snapshot = TimelineSnapshot::new(vec![segment(1, 1000, 2000, "a")]);
        assert!(snapshot.segment_at(500).is_none());
        assert_eq!(snapshot.segment_at(1500).expect("found").text, "a");
        assert_eq!(snapshot.duration_ms(), 2000);
    }

    #[test]
    fn clock_and_plain_text_formatting() {
        assert_eq!(format_clock(90_123), "01:30.123");
        let text = format_plain_text(&[segment(1, 1000, 2000, "hello")]);
        assert_eq!(text, "[00:01.000] hello\n");
    }
}
