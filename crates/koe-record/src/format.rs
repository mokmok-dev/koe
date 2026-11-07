//! The recording format registry.
//!
//! This module is the only place a format is described. Adding one means a
//! variant here, a row in `EXTENSIONS`, and one arm in the private `open`
//! factory in `file.rs`. The compiler reports the match arms; `EXTENSIONS` and
//! the test's `ALL` list are hand-kept, so a new variant needs both.

use std::path::Path;

use koe_record_core::{Error, RecordingSpec};

/// Container and codec a recording is written as.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum Format {
    /// Ogg with Opus audio.
    Ogg,
    /// Uncompressed RIFF/WAVE with 16-bit PCM audio.
    Wav,
}

/// Every name the registry claims, canonical one first.
const EXTENSIONS: [(Format, &[&str]); 2] = [
    (Format::Ogg, &["ogg", "oga", "opus"]),
    (Format::Wav, &["wav"]),
];

impl Format {
    /// File extension the format writes, without the dot.
    #[must_use]
    pub const fn extension(self) -> &'static str {
        match self {
            Self::Ogg => "ogg",
            Self::Wav => "wav",
        }
    }

    /// Resolves a file extension in either case.
    ///
    /// `oga` and `opus` name Ogg too, because players accept all three.
    #[must_use]
    pub fn from_extension(extension: &str) -> Option<Self> {
        EXTENSIONS
            .iter()
            .find(|(_, names)| {
                names
                    .iter()
                    .any(|name| extension.eq_ignore_ascii_case(name))
            })
            .map(|(format, _)| *format)
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
            // Every rate a `RecordingSpec` accepts fits WAV's `u32` rate field.
            Self::Wav => Ok(()),
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use koe_record_core::Channels;

    use super::*;

    const ALL: [Format; 2] = [Format::Ogg, Format::Wav];

    #[test]
    fn extensions_name_the_format_in_either_case() {
        for extension in ["ogg", "OGG", "Ogg", "oga", "opus", "OPUS"] {
            assert_eq!(
                Format::from_extension(extension),
                Some(Format::Ogg),
                "`{extension}` must name Ogg"
            );
        }
        for extension in ["wav", "WAV", "Wav"] {
            assert_eq!(
                Format::from_extension(extension),
                Some(Format::Wav),
                "`{extension}` must name WAV"
            );
        }
    }

    #[test]
    fn other_extensions_name_no_format() {
        for extension in ["wave", "mp3", "og", "", " ogg"] {
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
        assert_eq!(Format::from_path("take.wav"), Some(Format::Wav));
        assert_eq!(Format::from_path("/tmp/dir/take.WAV"), Some(Format::Wav));
        assert_eq!(Format::from_path("take"), None);
        assert_eq!(Format::from_path("take.mp3"), None);
        assert_eq!(Format::from_path(".ogg"), None);
        assert_eq!(Format::from_path(".wav"), None);
    }

    #[test]
    fn the_extension_round_trips() {
        for format in ALL {
            let extension = format.extension();
            assert_eq!(Format::from_extension(extension), Some(format));
            assert_eq!(
                Format::from_path(Path::new("take").with_extension(extension)),
                Some(format)
            );
        }
    }

    #[test]
    fn every_variant_has_a_row_and_every_row_names_a_variant() {
        for format in ALL {
            let row = EXTENSIONS
                .iter()
                .find(|(candidate, _)| *candidate == format);
            assert!(row.is_some(), "no `EXTENSIONS` row for {format:?}");
            if let Some((_, names)) = row {
                assert_eq!(
                    names.first(),
                    Some(&format.extension()),
                    "the canonical name leads the row for {format:?}"
                );
            }
        }
        for (format, _) in EXTENSIONS {
            assert!(
                ALL.contains(&format),
                "an `EXTENSIONS` row names {format:?}, which is not a variant"
            );
        }
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

    #[test]
    fn wav_accepts_every_rate_the_spec_allows() {
        for rate in [8_000, 11_025, 22_050, 44_100, 96_000, 192_000, 384_000] {
            for channels in [Channels::Mono, Channels::Stereo] {
                let spec = RecordingSpec::new(rate, channels).unwrap();
                assert_eq!(
                    Format::Wav.validate(spec),
                    Ok(()),
                    "{rate} Hz {channels:?} must be accepted"
                );
            }
        }
    }
}
