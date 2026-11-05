//! Reading 16-bit PCM RIFF/WAVE files.
//!
//! The transcription crates take interleaved samples and an [`AudioSpec`]. This
//! module is the only place the command turns a file into that pair.

use std::fmt;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use koe_transcribe::AudioSpec;

/// Decoded audio and its format.
#[derive(Debug, PartialEq)]
pub struct Audio {
    /// Interleaved samples in `-1.0..=1.0`.
    pub samples: Vec<f32>,
    /// Rate and channel layout.
    pub spec: AudioSpec,
}

/// Why a file is not a WAV this build reads.
#[derive(Debug)]
pub enum Error {
    /// The file could not be read.
    Io {
        /// The path the command tried to read.
        path: PathBuf,
        /// The underlying failure.
        source: io::Error,
    },
    /// The bytes are not RIFF/WAVE.
    NotWave,
    /// The format chunk is shorter than the 16 bytes `PCM` needs.
    ShortFormat,
    /// The file has no format chunk.
    NoFormat,
    /// The file has no data chunk.
    NoData,
    /// The encoding is not 16-bit `PCM`.
    Unsupported {
        /// WAVE format tag.
        encoding: u16,
        /// Bits per sample.
        bits: u16,
    },
    /// The format names a rate or channel count transcription cannot take.
    Spec(koe_transcribe::Error),
    /// The data chunk holds no samples.
    NoAudio,
}

impl fmt::Display for Error {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io { path, source } => write!(formatter, "read `{}`: {source}", path.display()),
            Self::NotWave => formatter.write_str("not a RIFF/WAVE file"),
            Self::ShortFormat => formatter.write_str("the format chunk is shorter than 16 bytes"),
            Self::NoFormat => formatter.write_str("the WAV file has no format chunk"),
            Self::NoData => formatter.write_str("the WAV file has no data chunk"),
            Self::Unsupported { encoding, bits } => write!(
                formatter,
                "unsupported WAV encoding {encoding} with {bits} bits per sample; only 16-bit PCM is supported"
            ),
            Self::Spec(error) => write!(formatter, "unsupported audio format: {error}"),
            Self::NoAudio => formatter.write_str("the WAV file has no audio"),
        }
    }
}

/// Reads `path` into interleaved samples.
///
/// # Errors
///
/// Returns [`Error::Io`] when the file cannot be read and the parse errors in
/// [`parse`] when its bytes are not 16-bit PCM WAVE.
pub fn read(path: &Path) -> Result<Audio, Error> {
    let bytes = fs::read(path).map_err(|source| Error::Io {
        path: path.to_path_buf(),
        source,
    })?;
    parse(&bytes)
}

/// Parses RIFF/WAVE bytes into interleaved samples.
///
/// A chunk that claims more bytes than the file holds is clamped to the file,
/// so a truncated `data` chunk still yields the samples that are present.
///
/// # Errors
///
/// Returns the first reason the bytes are not 16-bit `PCM` WAVE.
pub fn parse(bytes: &[u8]) -> Result<Audio, Error> {
    if bytes.len() < 12 || &bytes[..4] != b"RIFF" || &bytes[8..12] != b"WAVE" {
        return Err(Error::NotWave);
    }

    let mut format = None;
    let mut data = None;
    let mut cursor = 12;
    // A file cannot hold more chunks than it has bytes, and the bound stops a
    // malformed size from spinning the cursor in place.
    for _ in 0..bytes.len() {
        if cursor + 8 > bytes.len() {
            break;
        }
        let id = &bytes[cursor..cursor + 4];
        let size = u32::from_le_bytes([
            bytes[cursor + 4],
            bytes[cursor + 5],
            bytes[cursor + 6],
            bytes[cursor + 7],
        ]) as usize;
        let body = cursor + 8;
        let end = body.saturating_add(size).min(bytes.len());
        let chunk = &bytes[body..end];
        match id {
            b"fmt " => format = Some(parse_format(chunk)?),
            b"data" => data = Some(chunk),
            _ => {}
        }
        let Some(next) = advance(cursor, end, size) else {
            break;
        };
        cursor = next;
    }

    let (encoding, channels, rate, bits) = format.ok_or(Error::NoFormat)?;
    if encoding != 1 || bits != 16 {
        return Err(Error::Unsupported { encoding, bits });
    }
    let spec = AudioSpec::new(rate, channels).map_err(Error::Spec)?;
    let data = data.ok_or(Error::NoData)?;
    let samples: Vec<f32> = data
        .as_chunks::<2>()
        .0
        .iter()
        .map(|pair| f32::from(i16::from_le_bytes(*pair)) / 32_768.0)
        .collect();
    if samples.is_empty() {
        return Err(Error::NoAudio);
    }
    Ok(Audio { samples, spec })
}

const fn advance(cursor: usize, end: usize, size: usize) -> Option<usize> {
    let next = end.saturating_add(size & 1);
    if next > cursor { Some(next) } else { None }
}

const fn parse_format(chunk: &[u8]) -> Result<(u16, u16, u32, u16), Error> {
    if chunk.len() < 16 {
        return Err(Error::ShortFormat);
    }
    let encoding = u16::from_le_bytes([chunk[0], chunk[1]]);
    let channels = u16::from_le_bytes([chunk[2], chunk[3]]);
    let rate = u32::from_le_bytes([chunk[4], chunk[5], chunk[6], chunk[7]]);
    let bits = u16::from_le_bytes([chunk[14], chunk[15]]);
    Ok((encoding, channels, rate, bits))
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::float_cmp)]

    use super::*;

    fn format(encoding: u16, channels: u16, rate: u32, bits: u16) -> Vec<u8> {
        let mut body = Vec::with_capacity(16);
        body.extend_from_slice(&encoding.to_le_bytes());
        body.extend_from_slice(&channels.to_le_bytes());
        body.extend_from_slice(&rate.to_le_bytes());
        body.extend_from_slice(&[0x11, 0x22, 0x33, 0x44]);
        body.extend_from_slice(&[0x55, 0x66]);
        body.extend_from_slice(&bits.to_le_bytes());
        body
    }

    fn chunk(id: &[u8; 4], body: &[u8]) -> Vec<u8> {
        let mut out = Vec::with_capacity(8 + body.len() + 1);
        out.extend_from_slice(id);
        out.extend_from_slice(&u32::try_from(body.len()).unwrap().to_le_bytes());
        out.extend_from_slice(body);
        if body.len() % 2 == 1 {
            out.push(0);
        }
        out
    }

    fn riff(chunks: &[(&[u8; 4], &[u8])]) -> Vec<u8> {
        let mut out = Vec::new();
        out.extend_from_slice(b"RIFF");
        out.extend_from_slice(&0_u32.to_le_bytes());
        out.extend_from_slice(b"WAVE");
        for (id, body) in chunks {
            out.extend_from_slice(&chunk(id, body));
        }
        let size = u32::try_from(out.len() - 8).unwrap();
        out[4..8].copy_from_slice(&size.to_le_bytes());
        out
    }

    fn wav(encoding: u16, channels: u16, rate: u32, bits: u16, data: &[u8]) -> Vec<u8> {
        riff(&[
            (b"fmt ", &format(encoding, channels, rate, bits)),
            (b"data", data),
        ])
    }

    #[test]
    fn a_mono_file_decodes_to_samples() {
        let bytes = wav(1, 1, 48_000, 16, &[0x00, 0x00, 0xFF, 0x7F, 0x00, 0x80]);
        let audio = parse(&bytes).unwrap();
        assert_eq!(audio.spec, AudioSpec::new(48_000, 1).unwrap());
        assert_eq!(audio.samples, vec![0.0, 32_767.0 / 32_768.0, -1.0]);
    }

    #[test]
    fn a_stereo_file_keeps_the_interleaving() {
        let bytes = wav(1, 2, 44_100, 16, &[0x00, 0x40, 0x00, 0xC0]);
        let audio = parse(&bytes).unwrap();
        assert_eq!(audio.spec, AudioSpec::new(44_100, 2).unwrap());
        assert_eq!(audio.samples, vec![0.5, -0.5]);
    }

    #[test]
    fn unknown_chunks_before_the_format_are_skipped() {
        let bytes = riff(&[
            (b"LIST", b"info"),
            (b"fmt ", &format(1, 1, 16_000, 16)),
            (b"junk", &[1, 2, 3]),
            (b"data", &[0x10, 0x00]),
        ]);
        let audio = parse(&bytes).unwrap();
        assert_eq!(audio.spec, AudioSpec::new(16_000, 1).unwrap());
        assert_eq!(audio.samples.len(), 1);
    }

    #[test]
    fn a_truncated_data_chunk_yields_the_present_samples() {
        let mut bytes = wav(1, 1, 16_000, 16, &[0x10, 0x00, 0x20, 0x00]);
        let size_at = bytes.len() - 8;
        bytes[size_at..size_at + 4].copy_from_slice(&100_u32.to_le_bytes());
        let audio = parse(&bytes).unwrap();
        assert_eq!(audio.samples, vec![16.0 / 32_768.0, 32.0 / 32_768.0]);
    }

    #[test]
    fn a_data_chunk_stops_at_its_declared_size() {
        let bytes = riff(&[
            (b"fmt ", &format(1, 1, 16_000, 16)),
            (b"data", &[0x10, 0x00]),
            (b"JUNK", &[0x20, 0x00]),
        ]);
        let audio = parse(&bytes).unwrap();
        assert_eq!(audio.samples, vec![16.0 / 32_768.0]);
    }

    #[test]
    fn the_cursor_advances_by_the_chunk_and_its_padding() {
        assert_eq!(advance(12, 20, 0), Some(20));
        assert_eq!(advance(12, 12, 1), Some(13));
        assert_eq!(advance(0, usize::MAX, 1), Some(usize::MAX));
    }

    #[test]
    fn a_chunk_that_would_not_advance_stops_the_loop() {
        assert_eq!(advance(12, 12, 0), None);
        assert_eq!(advance(12, 11, 0), None);
    }

    #[test]
    fn a_trailing_chunk_header_with_a_zero_length_body_is_still_read() {
        let bytes = riff(&[(b"fmt ", &format(1, 1, 16_000, 16)), (b"data", &[])]);
        assert!(matches!(parse(&bytes), Err(Error::NoAudio)));
    }

    #[test]
    fn an_odd_data_length_ignores_the_final_byte() {
        let bytes = wav(1, 1, 16_000, 16, &[0x10, 0x00, 0x20]);
        let audio = parse(&bytes).unwrap();
        assert_eq!(audio.samples.len(), 1);
    }

    #[test]
    fn bytes_without_a_riff_header_are_rejected() {
        assert!(matches!(parse(b"RIFX\0\0\0\0WAVE"), Err(Error::NotWave)));
        assert!(matches!(parse(b"RIFF"), Err(Error::NotWave)));
        assert!(matches!(parse(b"RIFF\0\0\0\0WAVX"), Err(Error::NotWave)));
    }

    #[test]
    fn a_header_with_no_chunks_has_no_format() {
        assert!(matches!(parse(b"RIFF\0\0\0\0WAVE"), Err(Error::NoFormat)));
    }

    #[test]
    fn a_file_without_a_format_chunk_is_rejected() {
        let bytes = riff(&[(b"data", &[0x10, 0x00])]);
        assert!(matches!(parse(&bytes), Err(Error::NoFormat)));
    }

    #[test]
    fn a_file_without_a_data_chunk_is_rejected() {
        let bytes = riff(&[(b"fmt ", &format(1, 1, 16_000, 16))]);
        assert!(matches!(parse(&bytes), Err(Error::NoData)));
    }

    #[test]
    fn a_short_format_chunk_is_rejected() {
        let bytes = riff(&[
            (b"fmt ", &format(1, 1, 16_000, 16)[..14]),
            (b"data", &[0x10, 0x00]),
        ]);
        assert!(matches!(parse(&bytes), Err(Error::ShortFormat)));
    }

    #[test]
    fn a_non_pcm_encoding_is_rejected() {
        let bytes = wav(3, 1, 16_000, 32, &[0, 0, 0, 0]);
        assert!(matches!(
            parse(&bytes),
            Err(Error::Unsupported {
                encoding: 3,
                bits: 32
            })
        ));
    }

    #[test]
    fn a_non_16_bit_sample_is_rejected() {
        let bytes = wav(1, 1, 16_000, 24, &[0, 0, 0]);
        assert!(matches!(
            parse(&bytes),
            Err(Error::Unsupported {
                encoding: 1,
                bits: 24
            })
        ));
    }

    #[test]
    fn a_zero_channel_format_is_rejected() {
        let bytes = wav(1, 0, 16_000, 16, &[0x10, 0x00]);
        assert!(matches!(parse(&bytes), Err(Error::Spec(_))));
    }

    #[test]
    fn a_zero_rate_format_is_rejected() {
        let bytes = wav(1, 1, 0, 16, &[0x10, 0x00]);
        assert!(matches!(parse(&bytes), Err(Error::Spec(_))));
    }

    #[test]
    fn an_empty_data_chunk_is_rejected() {
        let bytes = wav(1, 1, 16_000, 16, &[]);
        assert!(matches!(parse(&bytes), Err(Error::NoAudio)));
    }

    #[test]
    fn reading_a_missing_file_names_the_path() {
        let error = read(Path::new("/nonexistent/koe-take.wav")).unwrap_err();
        match error {
            Error::Io { path, .. } => {
                assert_eq!(path, PathBuf::from("/nonexistent/koe-take.wav"));
            }
            other => panic!("expected an io error, got {other}"),
        }
    }

    #[test]
    fn reading_a_valid_file_decodes_it() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("take.wav");
        fs::write(&path, wav(1, 1, 16_000, 16, &[0x00, 0x40])).unwrap();
        let audio = read(&path).unwrap();
        assert_eq!(audio.spec, AudioSpec::new(16_000, 1).unwrap());
        assert_eq!(audio.samples, vec![0.5]);
    }

    #[test]
    fn every_error_has_a_message() {
        let cases = [
            (Error::NotWave, "not a RIFF/WAVE file"),
            (
                Error::ShortFormat,
                "the format chunk is shorter than 16 bytes",
            ),
            (Error::NoFormat, "the WAV file has no format chunk"),
            (Error::NoData, "the WAV file has no data chunk"),
            (Error::NoAudio, "the WAV file has no audio"),
        ];
        for (error, expected) in cases {
            assert_eq!(error.to_string(), expected);
        }
        assert_eq!(
            Error::Unsupported {
                encoding: 3,
                bits: 32
            }
            .to_string(),
            "unsupported WAV encoding 3 with 32 bits per sample; only 16-bit PCM is supported"
        );
        assert_eq!(
            Error::Spec(koe_transcribe::Error::InvalidSpec {
                sample_rate: 0,
                channels: 0
            })
            .to_string(),
            "unsupported audio format: unsupported audio spec: 0 Hz, 0 channels"
        );
        assert_eq!(
            Error::Io {
                path: PathBuf::from("/tmp/take.wav"),
                source: io::Error::from(io::ErrorKind::NotFound),
            }
            .to_string(),
            "read `/tmp/take.wav`: entity not found"
        );
    }
}
