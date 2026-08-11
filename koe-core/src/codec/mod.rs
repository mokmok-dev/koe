//! Audio encoding abstractions (FLAC follow-up in task 19).

mod wav;

#[cfg(feature = "ogg")]
mod ogg;

use koe_ffi::OutputFormat;
use thiserror::Error;

pub use wav::WavEncoder;

#[cfg(feature = "ogg")]
pub use ogg::{OggComments, OggEncoder};

/// Errors raised while encoding audio.
#[derive(Debug, Error)]
pub enum CodecError {
    #[error("Encoder error: {0}")]
    Encoder(String),
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
}

/// Encodes canonical PCM into a container.
///
/// Input is 48 kHz interleaved `f32`, typically stereo. WAV may be constructed
/// mono via [`WavEncoder::with_channels`]; the pipeline / [`create_encoder`] path
/// stays stereo.
pub trait AudioEncoder: Send {
    /// Encode a chunk of PCM audio.
    ///
    /// # Errors
    ///
    /// Returns [`CodecError`] when encoding fails.
    fn encode(
        &mut self,
        pcm: &[f32],
    ) -> Result<Vec<u8>, CodecError>;

    /// Flush buffered frames and write any container trailer.
    ///
    /// # Errors
    ///
    /// Returns [`CodecError`] when finalization fails.
    fn finalize(&mut self) -> Result<Vec<u8>, CodecError>;

    /// Output format descriptor for this encoder instance.
    fn format(&self) -> OutputFormat;

    /// Sample rate in Hz.
    fn sample_rate(&self) -> u32;

    /// Channel count.
    fn channel_count(&self) -> u16;
}

/// Creates an encoder for the requested output format.
///
/// When `comments` is `None` and the format is OGG, a minimal default comment
/// set is used. WAV/FLAC ignore comments.
///
/// # Errors
///
/// Returns [`CodecError`] when the format is unsupported or encoder setup fails.
pub fn create_encoder(
    format: &OutputFormat,
    comments: Option<&OggComments>,
) -> Result<Box<dyn AudioEncoder>, CodecError> {
    match format {
        OutputFormat::Wav { bits_per_sample } => Ok(Box::new(WavEncoder::new(*bits_per_sample)?)),
        OutputFormat::Ogg { quality } => {
            let comments = comments.cloned().unwrap_or_else(OggComments::basic);
            create_ogg_encoder(*quality, &comments)
        },
        OutputFormat::Flac { compression_level } => {
            Ok(Box::new(PlaceholderEncoder::flac(*compression_level)))
        },
    }
}

#[cfg(feature = "ogg")]
fn create_ogg_encoder(
    quality: f32,
    comments: &OggComments,
) -> Result<Box<dyn AudioEncoder>, CodecError> {
    Ok(Box::new(OggEncoder::with_comments(quality, comments)?))
}

#[cfg(not(feature = "ogg"))]
fn create_ogg_encoder(
    _quality: f32,
    _comments: &OggComments,
) -> Result<Box<dyn AudioEncoder>, CodecError> {
    Err(CodecError::Encoder(
        "OGG support requires the `ogg` feature".to_owned(),
    ))
}

#[cfg(not(feature = "ogg"))]
/// Vorbis Comment tags (no-op stub when the `ogg` feature is disabled).
#[derive(Debug, Clone, Default)]
pub struct OggComments;

#[cfg(not(feature = "ogg"))]
impl OggComments {
    /// Minimal tags used when no session metadata is available.
    #[must_use]
    pub fn basic() -> Self {
        Self
    }

    /// Builds session tags; ignored without the `ogg` feature.
    #[must_use]
    pub fn for_session(
        _source: &koe_ffi::AudioSourceConfig,
        _locale: &str,
    ) -> Self {
        Self
    }
}

/// Placeholder encoder until task 19 lands. Writes raw little-endian PCM.
struct PlaceholderEncoder {
    format: OutputFormat,
    sample_rate: u32,
    channel_count: u16,
}

impl PlaceholderEncoder {
    const fn flac(compression_level: u8) -> Self {
        Self {
            format: OutputFormat::Flac { compression_level },
            sample_rate: 48_000,
            channel_count: 2,
        }
    }
}

impl AudioEncoder for PlaceholderEncoder {
    fn encode(
        &mut self,
        pcm: &[f32],
    ) -> Result<Vec<u8>, CodecError> {
        let mut bytes = Vec::with_capacity(pcm.len() * 4);
        for sample in pcm {
            bytes.extend_from_slice(&sample.to_le_bytes());
        }
        Ok(bytes)
    }

    fn finalize(&mut self) -> Result<Vec<u8>, CodecError> {
        Ok(Vec::new())
    }

    fn format(&self) -> OutputFormat {
        self.format.clone()
    }

    fn sample_rate(&self) -> u32 {
        self.sample_rate
    }

    fn channel_count(&self) -> u16 {
        self.channel_count
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wav_encoder_writes_header_on_finalize() {
        let mut encoder = WavEncoder::new(16).expect("wav encoder");
        let _ = encoder.encode(&[0.0, 0.0]).expect("encode");
        let trailer = encoder.finalize().expect("finalize");
        assert!(trailer.len() >= 44);
    }

    #[test]
    fn create_encoder_wav_ignores_comments() {
        let encoder = create_encoder(
            &OutputFormat::Wav {
                bits_per_sample: 16,
            },
            None,
        )
        .expect("wav");
        assert_eq!(encoder.sample_rate(), 48_000);
        assert_eq!(encoder.channel_count(), 2);
    }

    #[cfg(feature = "ogg")]
    #[test]
    fn create_encoder_ogg_emits_ogg_capture_pattern() {
        let mut encoder = create_encoder(&OutputFormat::Ogg { quality: 0.4 }, None).expect("ogg");
        let pcm = vec![0.0_f32; 960 * 2];
        let mut out = encoder.encode(&pcm).expect("encode");
        out.extend(encoder.finalize().expect("finalize"));
        assert_eq!(&out[..4], b"OggS");
    }
}
