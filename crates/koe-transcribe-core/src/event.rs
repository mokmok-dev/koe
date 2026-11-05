//! Recognition events and the running text they build.

use alloc::boxed::Box;
use alloc::string::String;
use core::fmt;

/// One step of recognition, as a session reports it.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum Event {
    /// Text the engine will not revise.
    Committed {
        /// Committed text.
        text: Box<str>,
        /// Offset of the first sample the text covers, in milliseconds.
        start_ms: u64,
        /// Offset one past the last sample the text covers, in milliseconds.
        end_ms: u64,
    },
    /// A provisional tail the engine may replace or withdraw.
    Volatile {
        /// Current best guess for the uncommitted tail.
        text: Box<str>,
        /// Monotonic counter that changes with every volatile result.
        revision: u64,
    },
}

/// The text a stream of [`Event`]s adds up to.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct LiveText {
    committed: String,
    volatile: String,
    revision: u64,
}

impl LiveText {
    /// Creates empty text.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            committed: String::new(),
            volatile: String::new(),
            revision: 0,
        }
    }

    /// Folds one event into the accumulated text.
    pub fn apply(&mut self, event: &Event) {
        match event {
            Event::Committed { text, .. } => {
                self.committed.push_str(text);
                self.volatile.clear();
            }
            Event::Volatile { text, revision } => {
                self.volatile.clear();
                self.volatile.push_str(text);
                self.revision = *revision;
            }
        }
    }

    /// Text the engine will not revise.
    #[must_use]
    pub fn committed(&self) -> &str {
        &self.committed
    }

    /// Provisional tail, empty when the engine has committed everything.
    #[must_use]
    pub fn volatile(&self) -> &str {
        &self.volatile
    }

    /// Revision of the current volatile tail, zero before the first one.
    #[must_use]
    pub const fn revision(&self) -> u64 {
        self.revision
    }
}

impl fmt::Display for LiveText {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.committed)?;
        if !self.committed.is_empty() && !self.volatile.is_empty() {
            formatter.write_str(" ")?;
        }
        formatter.write_str(&self.volatile)
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;

    fn committed(text: &str, start_ms: u64, end_ms: u64) -> Event {
        Event::Committed {
            text: text.into(),
            start_ms,
            end_ms,
        }
    }

    fn volatile(text: &str, revision: u64) -> Event {
        Event::Volatile {
            text: text.into(),
            revision,
        }
    }

    #[test]
    fn committed_text_appends_and_clears_the_volatile_tail() {
        let mut live = LiveText::new();
        live.apply(&committed("hello", 0, 500));
        live.apply(&committed("world", 500, 900));
        live.apply(&volatile("ther", 3));
        live.apply(&committed(" there", 900, 1_200));

        assert_eq!(live.committed(), "helloworld there");
        assert_eq!(live.volatile(), "");
        assert_eq!(live.revision(), 3);
    }

    #[test]
    fn a_volatile_event_replaces_the_previous_tail() {
        let mut live = LiveText::new();
        live.apply(&volatile("hel", 1));
        assert_eq!(live.volatile(), "hel");
        live.apply(&volatile("hello", 2));
        assert_eq!(live.volatile(), "hello");
        assert_eq!(live.revision(), 2);
        assert_eq!(live.committed(), "");
    }

    #[test]
    fn display_joins_both_parts_with_one_space() {
        let mut live = LiveText::new();
        live.apply(&committed("hello", 0, 500));
        live.apply(&volatile("ther", 1));
        assert_eq!(live.to_string(), "hello ther");
    }

    #[test]
    fn display_omits_the_separator_when_a_part_is_empty() {
        assert_eq!(LiveText::new().to_string(), "");

        let mut only_committed = LiveText::new();
        only_committed.apply(&committed("hello", 0, 500));
        assert_eq!(only_committed.to_string(), "hello");

        let mut only_volatile = LiveText::new();
        only_volatile.apply(&volatile("hello", 1));
        assert_eq!(only_volatile.to_string(), "hello");
    }

    #[test]
    fn revision_is_zero_before_the_first_volatile_event() {
        assert_eq!(LiveText::new().revision(), 0);
        assert_eq!(LiveText::default().revision(), 0);
    }
}
