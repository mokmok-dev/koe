//! Sets of capture sources.

use koe_capture_core::Source;

const MICROPHONE_BIT: u8 = 1;
const SYSTEM_AUDIO_BIT: u8 = 1 << 1;

/// The sources this crate iterates, in the order every transition uses.
pub const SOURCES: [Source; 2] = [Source::Microphone, Source::SystemAudio];

/// Index of `source` in the per-source arrays of [`Permissions`][crate::Permissions]
/// and [`Transcript`][crate::Transcript].
pub const fn index(source: Source) -> usize {
    match source {
        Source::Microphone => 0,
        Source::SystemAudio => 1,
    }
}

/// A set of capture sources.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct SourceSet(u8);

impl SourceSet {
    /// The microphone alone.
    pub const MICROPHONE: Self = Self(MICROPHONE_BIT);

    /// The audio the machine plays alone.
    pub const SYSTEM_AUDIO: Self = Self(SYSTEM_AUDIO_BIT);

    /// Both sources, which is what a call needs.
    pub const MEETING: Self = Self(MICROPHONE_BIT | SYSTEM_AUDIO_BIT);

    /// No sources.
    pub(crate) const EMPTY: Self = Self(0);

    /// Whether `source` is a member.
    #[must_use]
    pub const fn contains(self, source: Source) -> bool {
        self.0 & Self::bit(source) != 0
    }

    /// Number of members.
    #[must_use]
    pub const fn len(self) -> usize {
        self.0.count_ones() as usize
    }

    /// Whether no source is a member.
    #[must_use]
    pub const fn is_empty(self) -> bool {
        self.0 == 0
    }

    /// The members of both sets.
    #[must_use]
    pub const fn union(self, other: Self) -> Self {
        Self(self.0 | other.0)
    }

    /// The members except `source`.
    #[must_use]
    pub const fn without(self, source: Source) -> Self {
        Self(self.0 & !Self::bit(source))
    }

    /// Iterates the members in canonical order.
    pub fn iter(self) -> impl Iterator<Item = Source> {
        SOURCES
            .into_iter()
            .filter(move |source| self.contains(*source))
    }

    /// The raw bit mask, in the encoding of [`Self::from_bits`].
    pub(crate) const fn bits(self) -> u8 {
        self.0
    }

    /// The set a wire mask names, or [`None`] when it names anything but the
    /// two capture sources.
    pub(crate) const fn from_bits(mask: u8) -> Option<Self> {
        if mask & !Self::MEETING.0 == 0 {
            Some(Self(mask))
        } else {
            None
        }
    }

    pub(crate) const fn bit(source: Source) -> u8 {
        match source {
            Source::Microphone => MICROPHONE_BIT,
            Source::SystemAudio => SYSTEM_AUDIO_BIT,
        }
    }
}

impl From<Source> for SourceSet {
    fn from(source: Source) -> Self {
        Self(Self::bit(source))
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;

    #[test]
    fn members_are_iterated_in_canonical_order() {
        assert_eq!(
            SourceSet::MEETING.iter().collect::<Vec<_>>(),
            vec![Source::Microphone, Source::SystemAudio]
        );
        assert_eq!(
            SourceSet::SYSTEM_AUDIO.iter().collect::<Vec<_>>(),
            vec![Source::SystemAudio]
        );
        assert_eq!(SourceSet::EMPTY.iter().count(), 0);
    }

    #[test]
    fn membership_and_len_agree() {
        assert!(SourceSet::MEETING.contains(Source::Microphone));
        assert!(SourceSet::MEETING.contains(Source::SystemAudio));
        assert!(!SourceSet::MICROPHONE.contains(Source::SystemAudio));
        assert!(!SourceSet::EMPTY.contains(Source::Microphone));
        assert_eq!(SourceSet::MEETING.len(), 2);
        assert_eq!(SourceSet::MICROPHONE.len(), 1);
        assert_eq!(SourceSet::SYSTEM_AUDIO.len(), 1);
        assert_eq!(SourceSet::EMPTY.len(), 0);
    }

    #[test]
    fn union_and_without_are_set_operations() {
        assert_eq!(
            SourceSet::MICROPHONE.union(Source::SystemAudio.into()),
            SourceSet::MEETING
        );
        assert_eq!(
            SourceSet::MEETING.without(Source::Microphone),
            SourceSet::SYSTEM_AUDIO
        );
        assert_eq!(
            SourceSet::MEETING.without(Source::SystemAudio),
            SourceSet::MICROPHONE
        );
        assert_eq!(
            SourceSet::MICROPHONE.without(Source::SystemAudio),
            SourceSet::MICROPHONE
        );
        assert_eq!(
            SourceSet::MICROPHONE.union(SourceSet::MICROPHONE),
            SourceSet::MICROPHONE
        );
        assert_eq!(
            SourceSet::MICROPHONE.without(Source::Microphone),
            SourceSet::EMPTY
        );
    }

    #[test]
    fn a_source_becomes_a_singleton() {
        assert_eq!(SourceSet::from(Source::Microphone), SourceSet::MICROPHONE);
        assert_eq!(
            SourceSet::from(Source::SystemAudio),
            SourceSet::SYSTEM_AUDIO
        );
    }

    #[test]
    fn every_source_has_its_own_index() {
        assert_eq!(index(Source::Microphone), 0);
        assert_eq!(index(Source::SystemAudio), 1);
        assert_eq!(SOURCES[index(Source::Microphone)], Source::Microphone);
        assert_eq!(SOURCES[index(Source::SystemAudio)], Source::SystemAudio);
    }

    #[test]
    fn wire_masks_round_trip_for_the_real_sets() {
        for set in [
            SourceSet::EMPTY,
            SourceSet::MICROPHONE,
            SourceSet::SYSTEM_AUDIO,
            SourceSet::MEETING,
        ] {
            assert_eq!(SourceSet::from_bits(set.bits()), Some(set));
        }
        assert_eq!(SourceSet::from_bits(0), Some(SourceSet::EMPTY));
        assert_eq!(SourceSet::from_bits(1), Some(SourceSet::MICROPHONE));
        assert_eq!(SourceSet::from_bits(2), Some(SourceSet::SYSTEM_AUDIO));
        assert_eq!(SourceSet::from_bits(3), Some(SourceSet::MEETING));
    }

    #[test]
    fn a_wire_mask_with_unknown_bits_is_rejected() {
        for mask in 4u8..=u8::MAX {
            assert_eq!(SourceSet::from_bits(mask), None);
        }
    }
}
