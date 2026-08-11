//! Audio encoding abstractions (full OGG/WAV/FLAC in tasks 17–19).

mod wav;

use koe_ffi::OutputFormat;
use thiserror::Error;

pub use wav::WavEncoder;

/// Errors raised while encoding audio.
#[derive(Debug, Error)]
pub enum CodecError {
    #[error("Encoder error: {0}")]
    Encoder(String),
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
}

/// Encodes canonical PCM (48 kHz, stereo, interleaved `f32`) into a container.
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
/// # Errors
///
/// Returns [`CodecError`] when the format is unsupported.
pub fn create_encoder(format: &OutputFormat) -> Result<Box<dyn AudioEncoder>, CodecError> {
    match format {
        OutputFormat::Wav { bits_per_sample } => {
            Ok(Box::new(WavEncoder::new(*bits_per_sample)?))
        },
        OutputFormat::Ogg { quality } => Ok(Box::new(PlaceholderEncoder::ogg(*quality))),
        OutputFormat::Flac { compression_level } => {
            Ok(Box::new(PlaceholderEncoder::flac(*compression_level)))
        },
    }
}

/// Placeholder encoder until tasks 17/19 land. Writes raw little-endian PCM.
struct PlaceholderEncoder {
    format: OutputFormat,
    sample_rate: u32,
    channel_count: u16,
}

impl PlaceholderEncoder {
    const fn ogg(quality: f32) -> Self {
        Self {
            format: OutputFormat::Ogg { quality },
            sample_rate: 48_000,
            channel_count: 2,
        }
    }

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
}
