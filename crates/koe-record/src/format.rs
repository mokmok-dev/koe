//! The recording format registry.
//!
//! This module is the only place a format is described. Adding one means a
//! variant here, its extension arms, and one arm in the private `open` factory
//! in `file.rs`; the compiler reports every other place that needs to change.

use std::path::Path;

use koe_record_core::{Error, RecordingSpec};

/// Container and codec a recording is written as.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum Format {
    /// Ogg with Opus audio.
    Ogg,
}

impl Format {
    /// File extension the format writes, without the dot.
    #[must_use]
    pub const fn extension(self) -> &'static str {
        match self {
            Self::Ogg => "ogg",
        }
    }

    /// Resolves a file extension in either case.
    ///
    /// `oga` and `opus` name Ogg too, because players accept all three.
    #[must_use]
    pub const fn from_extension(extension: &str) -> Option<Self> {
        let is_ogg = extension.eq_ignore_ascii_case("ogg")
            || extension.eq_ignore_ascii_case("oga")
            || extension.eq_ignore_ascii_case("opus");
        if is_ogg { Some(Self::Ogg) } else { None }
    }

    /// Resolves the format a path names.
    #[must_use]
    pub fn from_path(path: impl AsRef<Path>) -> Option<Self> {
        let extension = path.as_ref().extension()?.to_str()?;
        Self::from_extension(extension)
    }

    /// Rejects a spec the format cannot carry.
    pub(crate) const fn validate(self, spec: RecordingSpec) -> Result<(), Error> {
        match self {
            Self::Ogg => {
                let rate = spec.sample_rate();
                // Opus codes audio at 8, 12, 16, 24, or 48 kHz.
                if matches!(rate, 8_000 | 12_000 | 16_000 | 24_000 | 48_000) {
                    Ok(())
                } else {
                    Err(Error::UnsupportedRate { rate })
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use koe_record_core::Channels;

    use super::*;

    #[test]
    fn extensions_name_the_format_in_either_case() {
        for extension in ["ogg", "OGG", "Ogg", "oga", "opus", "OPUS"] {
            assert_eq!(
                Format::from_extension(extension),
                Some(Format::Ogg),
                "`{extension}` must name Ogg"
            );
        }
    }

    #[test]
    fn other_extensions_name_no_format() {
        for extension in ["wav", "mp3", "og", "", " ogg"] {
            assert_eq!(
                Format::from_extension(extension),
                None,
                "`{extension}` must name no format"
            );
        }
    }

    #[test]
    fn paths_are_resolved_by_extension() {
        assert_eq!(Format::from_path("take.ogg"), Some(Format::Ogg));
        assert_eq!(Format::from_path("/tmp/dir/take.OPUS"), Some(Format::Ogg));
        assert_eq!(Format::from_path("take"), None);
        assert_eq!(Format::from_path("take.wav"), None);
        assert_eq!(Format::from_path(".ogg"), None);
    }

    #[test]
    fn the_extension_round_trips() {
        assert_eq!(Format::Ogg.extension(), "ogg");
        assert_eq!(
            Format::from_path(Path::new("take").with_extension(Format::Ogg.extension())),
            Some(Format::Ogg)
        );
    }

    #[test]
    fn ogg_accepts_the_opus_rates() {
        for rate in [8_000, 12_000, 16_000, 24_000, 48_000] {
            let spec = RecordingSpec::new(rate, Channels::Stereo).unwrap();
            assert_eq!(
                Format::Ogg.validate(spec),
                Ok(()),
                "{rate} Hz must be accepted"
            );
        }
    }

    #[test]
    fn ogg_rejects_other_rates() {
        for rate in [11_025, 22_050, 32_000, 44_100, 96_000] {
            let spec = RecordingSpec::new(rate, Channels::Mono).unwrap();
            assert_eq!(
                Format::Ogg.validate(spec),
                Err(Error::UnsupportedRate { rate }),
                "{rate} Hz must be rejected"
            );
        }
    }
}
