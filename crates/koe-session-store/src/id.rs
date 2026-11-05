//! Meeting identity.

use std::fmt;
use std::time::{SystemTime, UNIX_EPOCH};

/// Number of decimal digits [`SessionId`] displays.
const DIGITS: usize = 20;

/// The identity of one stored meeting.
///
/// The value is the number of milliseconds since the Unix epoch and is also
/// the name of the meeting's directory.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct SessionId(u64);

impl SessionId {
    /// Names the meeting created at `millis` milliseconds after the Unix epoch.
    #[must_use]
    pub const fn from_millis(millis: u64) -> Self {
        Self(millis)
    }

    /// Milliseconds since the Unix epoch.
    #[must_use]
    pub const fn as_millis(self) -> u64 {
        self.0
    }

    /// The id of the meeting created at `created_at`; a time before the epoch
    /// reads as the epoch.
    #[must_use]
    pub fn of(created_at: SystemTime) -> Self {
        let millis = created_at.duration_since(UNIX_EPOCH).map_or(0, |elapsed| {
            u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX)
        });
        Self(millis)
    }

    /// Reads a directory name, accepting exactly twenty ASCII digits.
    pub fn parse(name: &str) -> Option<Self> {
        if name.len() != DIGITS {
            return None;
        }
        if !name.bytes().all(|byte| byte.is_ascii_digit()) {
            return None;
        }
        name.parse().ok().map(Self)
    }
}

impl fmt::Display for SessionId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{:020}", self.0)
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use std::time::Duration;

    use super::*;

    #[test]
    fn an_id_round_trips_through_its_directory_name() {
        for id in [
            SessionId::from_millis(0),
            SessionId::from_millis(1_234_567),
            SessionId::from_millis(u64::MAX),
        ] {
            assert_eq!(SessionId::parse(&id.to_string()), Some(id));
        }
    }

    #[test]
    fn a_name_that_is_not_twenty_digits_is_rejected() {
        assert_eq!(SessionId::parse(""), None);
        assert_eq!(SessionId::parse("123"), None);
        assert_eq!(SessionId::parse("0000000000000000000"), None);
        assert_eq!(SessionId::parse("000000000000000000001"), None);
        assert_eq!(SessionId::parse("0000000000000000000A"), None);
        assert_eq!(SessionId::parse("0000000000000000000 "), None);
    }

    #[test]
    fn a_twenty_digit_name_that_overflows_is_rejected() {
        assert_eq!(SessionId::parse("18446744073709551616"), None);
    }

    #[test]
    fn a_time_before_the_epoch_is_the_epoch() {
        let before = UNIX_EPOCH.checked_sub(Duration::from_millis(5)).unwrap();
        assert_eq!(SessionId::of(before), SessionId::from_millis(0));
        assert_eq!(SessionId::of(UNIX_EPOCH), SessionId::from_millis(0));
        assert_eq!(
            SessionId::of(UNIX_EPOCH + Duration::from_millis(5)),
            SessionId::from_millis(5)
        );
    }
}
