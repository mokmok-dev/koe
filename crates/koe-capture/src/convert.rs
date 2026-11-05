//! Sample decoding into the interleaved `f32` form the crate returns.
//!
//! Only the cfg-gated platform adapters call this in a release build. The
//! core-only build keeps it compiled so property tests and mutation testing
//! still cover it.

#![allow(dead_code)]

use crate::error::{Error, Result};

/// Sample layouts a platform backend can hand back.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SampleFormat {
    /// Signed 16-bit integers in native byte order.
    I16,
    /// Signed 32-bit integers in native byte order.
    I32,
    /// 32-bit floats in native byte order.
    F32,
}

impl SampleFormat {
    /// Bytes occupied by one sample.
    pub const fn bytes_per_sample(self) -> usize {
        match self {
            Self::I16 => 2,
            Self::I32 | Self::F32 => 4,
        }
    }
}

/// Decodes native-endian samples into `f32` values.
///
/// Integer samples are scaled by their full-scale magnitude, so `i16::MIN`
/// maps to `-1.0` and `i16::MAX` maps to `1.0 - 1 / 32768`.
///
/// # Errors
///
/// Returns [`Error::MisalignedBuffer`] when `bytes` does not contain whole
/// samples for `format`.
#[allow(clippy::cast_precision_loss)]
pub fn decode(bytes: &[u8], format: SampleFormat, out: &mut [f32]) -> Result<usize> {
    let sample_size = format.bytes_per_sample();
    if !bytes.len().is_multiple_of(sample_size) {
        return Err(Error::MisalignedBuffer {
            len: bytes.len(),
            channels: sample_size,
        });
    }
    let samples = bytes.len() / sample_size;
    if samples > out.len() {
        return Err(Error::OutputTooSmall {
            samples,
            capacity: out.len(),
        });
    }

    for (index, chunk) in bytes.chunks_exact(sample_size).enumerate() {
        out[index] = match format {
            SampleFormat::I16 => f32::from(i16::from_ne_bytes([chunk[0], chunk[1]])) / 32_768.0,
            SampleFormat::I32 => {
                i32::from_ne_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]) as f32
                    / 2_147_483_648.0
            }
            SampleFormat::F32 => f32::from_ne_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]),
        };
    }
    Ok(samples)
}

#[cfg(test)]
mod tests {
    #![allow(
        clippy::unwrap_used,
        clippy::float_cmp,
        clippy::cast_precision_loss,
        clippy::needless_range_loop
    )]

    use super::*;
    use proptest::prelude::*;

    #[test]
    fn sample_sizes_are_exact() {
        assert_eq!(SampleFormat::I16.bytes_per_sample(), 2);
        assert_eq!(SampleFormat::I32.bytes_per_sample(), 4);
        assert_eq!(SampleFormat::F32.bytes_per_sample(), 4);
    }

    #[test]
    fn decodes_i16_full_scale() {
        let bytes = [
            i16::MIN.to_ne_bytes(),
            (-1i16).to_ne_bytes(),
            0i16.to_ne_bytes(),
            1i16.to_ne_bytes(),
            i16::MAX.to_ne_bytes(),
        ]
        .concat();
        let mut out = [0.0; 5];
        assert_eq!(decode(&bytes, SampleFormat::I16, &mut out).unwrap(), 5);
        assert_eq!(out[0], -1.0);
        assert_eq!(out[1], -1.0 / 32_768.0);
        assert_eq!(out[2], 0.0);
        assert_eq!(out[3], 1.0 / 32_768.0);
        assert_eq!(out[4], (32_767.0) / 32_768.0);
    }

    #[test]
    fn decodes_i32_full_scale() {
        let bytes = [
            i32::MIN.to_ne_bytes(),
            0i32.to_ne_bytes(),
            i32::MAX.to_ne_bytes(),
        ]
        .concat();
        let mut out = [0.0; 3];
        assert_eq!(decode(&bytes, SampleFormat::I32, &mut out).unwrap(), 3);
        assert_eq!(out[0], -1.0);
        assert_eq!(out[1], 0.0);
        assert_eq!(out[2], 1.0);
    }

    #[test]
    fn decodes_f32_verbatim() {
        let bytes = [0.0f32, 0.5, -0.5, 1.0].map(f32::to_ne_bytes).concat();
        let mut out = [0.0; 4];
        assert_eq!(decode(&bytes, SampleFormat::F32, &mut out).unwrap(), 4);
        assert_eq!(out, [0.0, 0.5, -0.5, 1.0]);
    }

    #[test]
    fn rejects_trailing_partial_samples() {
        assert_eq!(
            decode(&[0, 1, 2], SampleFormat::I16, &mut [0.0; 2]),
            Err(Error::MisalignedBuffer {
                len: 3,
                channels: 2
            })
        );
        assert_eq!(
            decode(&[0, 1, 2], SampleFormat::F32, &mut [0.0; 4]),
            Err(Error::MisalignedBuffer {
                len: 3,
                channels: 4
            })
        );
    }

    #[test]
    fn rejects_output_smaller_than_the_input() {
        assert_eq!(
            decode(&[0, 0, 0, 0], SampleFormat::I16, &mut [0.0; 1],),
            Err(Error::OutputTooSmall {
                samples: 2,
                capacity: 1,
            })
        );
    }

    #[test]
    fn empty_input_decodes_nothing() {
        let mut out = [1.0; 1];
        assert_eq!(decode(&[], SampleFormat::F32, &mut out).unwrap(), 0);
        assert_eq!(out, [1.0]);
    }

    proptest! {
        #[test]
        fn i16_decoding_matches_full_scale_scaling(
            values in proptest::collection::vec(any::<i16>(), 0..64)
        ) {
            let bytes: Vec<u8> = values.iter().flat_map(|value| value.to_ne_bytes()).collect();
            let mut out = vec![0.0; values.len()];
            let decoded = decode(&bytes, SampleFormat::I16, &mut out).unwrap();
            prop_assert_eq!(decoded, values.len());
            for (actual, expected) in out.iter().zip(values.iter()) {
                prop_assert_eq!(*actual, f32::from(*expected) / 32_768.0);
            }
        }

        #[test]
        fn f32_decoding_round_trips(
            values in proptest::collection::vec(any::<f32>(), 0..64)
        ) {
            let bytes: Vec<u8> = values.iter().flat_map(|value| value.to_ne_bytes()).collect();
            let mut out = vec![0.0; values.len()];
            let decoded = decode(&bytes, SampleFormat::F32, &mut out).unwrap();
            prop_assert_eq!(decoded, values.len());
            prop_assert_eq!(&out, &values);
        }

        #[test]
        fn every_whole_sample_count_decodes(
            count in 0usize..64,
            extra in 1usize..4,
        ) {
            let bytes = vec![0u8; count * 4 + extra];
            prop_assert!(decode(&bytes, SampleFormat::F32, &mut vec![0.0; count + 1]).is_err());
        }
    }
}
