//! Minimal WAV writer (full implementation in task 18).

use koe_ffi::OutputFormat;

use super::{AudioEncoder, CodecError};

const SAMPLE_RATE: u32 = 48_000;
const CHANNELS: u16 = 2;

/// Writes canonical stereo PCM into a RIFF/WAVE container.
pub struct WavEncoder {
    bits_per_sample: u16,
    pcm_bytes: Vec<u8>,
    header_written: bool,
}

impl WavEncoder {
    /// Creates a WAV encoder for the given bit depth.
    ///
    /// # Errors
    ///
    /// Returns [`CodecError::Encoder`] for unsupported bit depths.
    pub fn new(bits_per_sample: u16) -> Result<Self, CodecError> {
        if !matches!(bits_per_sample, 16 | 24 | 32) {
            return Err(CodecError::Encoder(format!(
                "unsupported WAV bit depth: {bits_per_sample}"
            )));
        }
        Ok(Self {
            bits_per_sample,
            pcm_bytes: Vec::new(),
            header_written: false,
        })
    }

    const fn bytes_per_sample(&self) -> u16 {
        self.bits_per_sample / 8
    }

    fn quantize_sample(
        &self,
        sample: f32,
    ) -> Vec<u8> {
        let clamped = sample.clamp(-1.0, 1.0);
        match self.bits_per_sample {
            16 => {
                #[allow(clippy::cast_possible_truncation)]
                let value = (clamped * f32::from(i16::MAX)) as i16;
                value.to_le_bytes().to_vec()
            },
            24 => {
                #[allow(clippy::cast_possible_truncation)]
                let value = (clamped * 8_388_607.0) as i32;
                let bytes = value.to_le_bytes();
                bytes[..3].to_vec()
            },
            32 => clamped.to_le_bytes().to_vec(),
            _ => Vec::new(),
        }
    }

    fn build_header(
        &self,
        data_size: u32,
    ) -> Vec<u8> {
        let byte_rate = SAMPLE_RATE * u32::from(CHANNELS) * u32::from(self.bytes_per_sample());
        let block_align = CHANNELS * self.bytes_per_sample();
        let mut header = Vec::with_capacity(44);
        header.extend_from_slice(b"RIFF");
        header.extend_from_slice(&(36 + data_size).to_le_bytes());
        header.extend_from_slice(b"WAVE");
        header.extend_from_slice(b"fmt ");
        header.extend_from_slice(&16u32.to_le_bytes());
        header.extend_from_slice(&1u16.to_le_bytes()); // PCM
        header.extend_from_slice(&CHANNELS.to_le_bytes());
        header.extend_from_slice(&SAMPLE_RATE.to_le_bytes());
        header.extend_from_slice(&byte_rate.to_le_bytes());
        header.extend_from_slice(&block_align.to_le_bytes());
        header.extend_from_slice(&self.bits_per_sample.to_le_bytes());
        header.extend_from_slice(b"data");
        header.extend_from_slice(&data_size.to_le_bytes());
        header
    }
}

impl AudioEncoder for WavEncoder {
    fn encode(
        &mut self,
        pcm: &[f32],
    ) -> Result<Vec<u8>, CodecError> {
        for sample in pcm {
            self.pcm_bytes
                .extend_from_slice(&self.quantize_sample(*sample));
        }
        Ok(Vec::new())
    }

    fn finalize(&mut self) -> Result<Vec<u8>, CodecError> {
        if self.header_written {
            return Ok(Vec::new());
        }
        self.header_written = true;
        let data_size = u32::try_from(self.pcm_bytes.len())
            .map_err(|_| CodecError::Encoder("WAV payload exceeds u32::MAX".to_owned()))?;
        let mut out = self.build_header(data_size);
        out.append(&mut self.pcm_bytes);
        Ok(out)
    }

    fn format(&self) -> OutputFormat {
        OutputFormat::Wav {
            bits_per_sample: self.bits_per_sample,
        }
    }

    fn sample_rate(&self) -> u32 {
        SAMPLE_RATE
    }

    fn channel_count(&self) -> u16 {
        CHANNELS
    }
}
