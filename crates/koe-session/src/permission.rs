//! Microphone and system audio permission state.

use core::fmt;

use koe_capture_core::Source;

use crate::sources::{SourceSet, index};

/// What the operating system currently allows for one source.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Permission {
    /// Never asked.
    Unknown,
    /// Asked, and the user has not answered yet.
    Requested,
    /// Allowed.
    Granted,
    /// Refused.
    Denied,
}

impl Permission {
    /// Whether capture may run now.
    #[must_use]
    pub const fn is_granted(self) -> bool {
        matches!(self, Self::Granted)
    }

    /// Whether a request may be made now.
    #[must_use]
    pub const fn can_request(self) -> bool {
        matches!(self, Self::Unknown | Self::Denied)
    }

    /// Lowercase name used in diagnostics.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Unknown => "unknown",
            Self::Requested => "requested",
            Self::Granted => "granted",
            Self::Denied => "denied",
        }
    }
}

impl fmt::Display for Permission {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// The permission of every source.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Permissions([Permission; 2]);

impl Permissions {
    /// Builds the pair from the microphone and system audio answers.
    #[must_use]
    pub const fn new(microphone: Permission, system_audio: Permission) -> Self {
        Self([microphone, system_audio])
    }

    /// The permission of `source`.
    #[must_use]
    pub const fn get(self, source: Source) -> Permission {
        self.0[index(source)]
    }

    /// Whether every member of `sources` is granted.
    #[must_use]
    pub const fn all_granted(self, sources: SourceSet) -> bool {
        (!sources.contains(Source::Microphone) || self.0[0].is_granted())
            && (!sources.contains(Source::SystemAudio) || self.0[1].is_granted())
    }

    pub(crate) const fn set(&mut self, source: Source, permission: Permission) {
        self.0[index(source)] = permission;
    }
}

impl Default for Permissions {
    fn default() -> Self {
        Self([Permission::Unknown, Permission::Unknown])
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;

    #[test]
    fn permission_names_are_distinct() {
        assert_eq!(Permission::Unknown.as_str(), "unknown");
        assert_eq!(Permission::Requested.as_str(), "requested");
        assert_eq!(Permission::Granted.as_str(), "granted");
        assert_eq!(Permission::Denied.as_str(), "denied");
        assert_eq!(Permission::Granted.to_string(), "granted");
    }

    #[test]
    fn only_granted_allows_capture() {
        assert!(Permission::Granted.is_granted());
        assert!(!Permission::Unknown.is_granted());
        assert!(!Permission::Requested.is_granted());
        assert!(!Permission::Denied.is_granted());
    }

    #[test]
    fn only_unknown_and_denied_can_be_requested() {
        assert!(Permission::Unknown.can_request());
        assert!(Permission::Denied.can_request());
        assert!(!Permission::Requested.can_request());
        assert!(!Permission::Granted.can_request());
    }

    #[test]
    fn default_is_unknown_for_both_sources() {
        let permissions = Permissions::default();
        assert_eq!(permissions.get(Source::Microphone), Permission::Unknown);
        assert_eq!(permissions.get(Source::SystemAudio), Permission::Unknown);
    }

    #[test]
    fn permission_is_read_per_source() {
        let permissions = Permissions::new(Permission::Granted, Permission::Denied);
        assert_eq!(permissions.get(Source::Microphone), Permission::Granted);
        assert_eq!(permissions.get(Source::SystemAudio), Permission::Denied);

        let mut permissions = permissions;
        permissions.set(Source::SystemAudio, Permission::Granted);
        assert_eq!(permissions.get(Source::SystemAudio), Permission::Granted);
    }

    #[test]
    fn all_granted_covers_exactly_the_requested_sources() {
        let granted = Permissions::new(Permission::Granted, Permission::Granted);
        assert!(granted.all_granted(SourceSet::MEETING));
        assert!(granted.all_granted(SourceSet::MICROPHONE));
        assert!(granted.all_granted(SourceSet::EMPTY));

        let mixed = Permissions::new(Permission::Granted, Permission::Requested);
        assert!(mixed.all_granted(SourceSet::MICROPHONE));
        assert!(mixed.all_granted(SourceSet::EMPTY));
        assert!(!mixed.all_granted(SourceSet::SYSTEM_AUDIO));
        assert!(!mixed.all_granted(SourceSet::MEETING));
    }
}
