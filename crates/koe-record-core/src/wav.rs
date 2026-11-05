//! RIFF/WAVE framing for 16-bit PCM audio.

use alloc::vec::Vec;

use crate::error::Error;
use crate::sink::PatchSink;
use crate::spec::RecordingSpec;

/// Header bytes the RIFF size field counts.
const RIFF_PREFIX: u32 = 36;

/// Bytes the encoded header occupies; the first sample starts here.
const HEADER_LEN: usize = 44;

/// WAVE format tag for PCM.
const PCM_FORMAT: u16 = 1;
const BITS_PER_SAMPLE: u16 = 16;
const SAMPLE_BYTES: u16 = 2;

/// Scale that maps a full-scale sample into the 16-bit range.
const PCM_SCALE: f32 = 32_768.0;

/// Most sample bytes the RIFF size field can describe.
const MAX_DATA_BYTES: u64 = (u32::MAX - RIFF_PREFIX) as u64;

/// The two lengths a RIFF header records.
///
/// One value owns both, so `riff` always counts `RIFF_PREFIX` more than `data`
/// and neither can leave `u32`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct ChunkSizes {
    data: u32,
    /// Bytes from offset 8 to the end of the file.
    riff: u32,
}

impl ChunkSizes {
    const EMPTY: Self = Self {
        data: 0,
        riff: RIFF_PREFIX,
    };

    /// Records `bytes` more sample data, refusing a total `limit` cannot hold.
    ///
    /// `limit` is the most sample bytes the header's size field can describe.
    fn grow(self, bytes: u64, limit: u64) -> Result<Self, Error> {
        // The size field is the hard ceiling, so a limit above it is capped.
        let limit = limit.min(MAX_DATA_BYTES);
        let data = u64::from(self.data)
            .checked_add(bytes)
            .ok_or(Error::ContainerInvariant {
                operation: "data size",
            })?;
        if data > limit {
            return Err(Error::RecordingTooLarge {
                bytes: data,
                max: limit,
            });
        }
        let data = u32::try_from(data).map_err(|_| Error::ContainerInvariant {
            operation: "data size",
        })?;
        // `limit <= MAX_DATA_BYTES`, so the RIFF total stays inside `u32`.
        Ok(Self {
            data,
            riff: data + RIFF_PREFIX,
        })
    }
}

/// The fixed part of the header, everything except the two lengths.
#[derive(Clone, Copy, Debug)]
struct WavHeader {
    channels: u16,
    sample_rate: u32,
}

impl WavHeader {
    const fn new(spec: RecordingSpec) -> Self {
        Self {
            channels: spec.channels().count(),
            sample_rate: spec.sample_rate(),
        }
    }

    /// Emits the whole header with `sizes` in its two length fields.
    ///
    /// The placeholder header at the start and the final header at the end
    /// come from this encoder, so the byte layout is written in one place.
    fn encode(self, sizes: ChunkSizes) -> [u8; HEADER_LEN] {
        let block_align = self.channels * SAMPLE_BYTES;
        // `RecordingSpec` caps the rate at 384 kHz, so the product fits `u32`.
        let byte_rate = self.sample_rate * u32::from(block_align);
        let mut out = [0_u8; HEADER_LEN];
        let mut at = 0;
        let mut put = |bytes: &[u8]| {
            let end = at + bytes.len();
            out[at..end].copy_from_slice(bytes);
            at = end;
        };
        put(b"RIFF");
        put(&sizes.riff.to_le_bytes());
        put(b"WAVE");
        put(b"fmt ");
        put(&16_u32.to_le_bytes());
        put(&PCM_FORMAT.to_le_bytes());
        put(&self.channels.to_le_bytes());
        put(&self.sample_rate.to_le_bytes());
        put(&byte_rate.to_le_bytes());
        put(&block_align.to_le_bytes());
        put(&BITS_PER_SAMPLE.to_le_bytes());
        put(b"data");
        put(&sizes.data.to_le_bytes());
        out
    }
}

/// Writes a canonical 16-bit PCM RIFF/WAVE file.
///
/// The header goes out with zero lengths when the writer starts and is written
/// again, whole, from the same encoder when the writer finishes. At most one
/// block of quantized samples is held, so the audio reaches the sink as it is
/// handed over.
pub struct WavWriter<S: PatchSink> {
    sink: S,
    header: WavHeader,
    sizes: ChunkSizes,
    limit: u64,
    bytes: Vec<u8>,
}

impl<S: PatchSink> WavWriter<S> {
    /// Starts a file at `sink`, writing a header with an empty data chunk.
    ///
    /// # Errors
    ///
    /// Returns the sink's error when the header cannot be written.
    pub fn new(sink: S, spec: RecordingSpec) -> Result<Self, Error> {
        Self::with_limit(sink, spec, MAX_DATA_BYTES)
    }

    /// Starts a file whose data chunk holds at most `limit` bytes.
    ///
    /// `new` passes the format's own limit. A smaller one lets a test walk the
    /// oversize path with two frames instead of two hours of audio.
    ///
    /// # Errors
    ///
    /// Returns the sink's error when the header cannot be written.
    fn with_limit(mut sink: S, spec: RecordingSpec, limit: u64) -> Result<Self, Error> {
        let header = WavHeader::new(spec);
        sink.write_all(&header.encode(ChunkSizes::EMPTY))?;
        Ok(Self {
            sink,
            header,
            sizes: ChunkSizes::EMPTY,
            limit,
            bytes: Vec::new(),
        })
    }

    /// Appends one block of interleaved samples holding whole frames.
    ///
    /// # Errors
    ///
    /// Returns [`Error::MisalignedBuffer`] for a partial frame,
    /// [`Error::RecordingTooLarge`] when the block would outgrow the data
    /// chunk's size field, and the sink's error when the block cannot be
    /// written. A refused block leaves the header describing what landed.
    pub fn write(&mut self, samples: &[f32]) -> Result<(), Error> {
        let channels = usize::from(self.header.channels);
        if !samples.len().is_multiple_of(channels) {
            return Err(Error::MisalignedBuffer {
                len: samples.len(),
                channels,
            });
        }
        let bytes = u64::try_from(samples.len())
            .ok()
            .and_then(|len| len.checked_mul(u64::from(SAMPLE_BYTES)))
            .ok_or(Error::ContainerInvariant {
                operation: "write block",
            })?;
        let sizes = self.sizes.grow(bytes, self.limit)?;

        self.bytes.clear();
        for sample in samples {
            self.bytes.extend_from_slice(&pcm16(*sample).to_le_bytes());
        }
        self.sink.write_all(&self.bytes)?;
        // Committed only after the sink took every byte, so the header never
        // claims audio the sink did not accept.
        self.sizes = sizes;
        Ok(())
    }

    /// Rewrites the header with the final lengths and returns the sink.
    ///
    /// # Errors
    ///
    /// Returns the sink's error when the header cannot be rewritten.
    pub fn finish(self) -> Result<S, Error> {
        let Self {
            mut sink,
            header,
            sizes,
            ..
        } = self;
        sink.write_at(0, &header.encode(sizes))?;
        Ok(sink)
    }
}

/// Quantizes one sample to 16-bit PCM.
///
/// `f32::round` lives in `std`, so a half-step bias before a cast that
/// truncates toward zero rounds half away from zero.
#[allow(clippy::cast_possible_truncation)] // the clamp bounds the value to the i16 range
fn pcm16(sample: f32) -> i16 {
    if !sample.is_finite() {
        return 0;
    }
    let scaled = sample * PCM_SCALE;
    let biased = if scaled >= 0.0 {
        scaled + 0.5
    } else {
        scaled - 0.5
    };
    biased.clamp(f32::from(i16::MIN), f32::from(i16::MAX)) as i16
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use alloc::vec;

    use super::*;
    use crate::sink::Sink;
    use crate::spec::Channels;

    #[derive(Default)]
    struct TestSink {
        bytes: Vec<u8>,
        appends: Vec<usize>,
        patches: Vec<(usize, Vec<u8>)>,
    }

    impl Sink for TestSink {
        fn write_all(&mut self, bytes: &[u8]) -> Result<(), Error> {
            self.appends.push(bytes.len());
            self.bytes.extend_from_slice(bytes);
            Ok(())
        }
    }

    impl PatchSink for TestSink {
        fn write_at(&mut self, offset: usize, bytes: &[u8]) -> Result<(), Error> {
            self.patches.push((offset, bytes.to_vec()));
            self.bytes[offset..offset + bytes.len()].copy_from_slice(bytes);
            Ok(())
        }
    }

    /// A sink that accepts `accepted` calls and fails every later one.
    struct FailingSink {
        bytes: Vec<u8>,
        calls: usize,
        accepted: usize,
    }

    impl Sink for FailingSink {
        fn write_all(&mut self, bytes: &[u8]) -> Result<(), Error> {
            self.calls += 1;
            if self.calls > self.accepted {
                return Err(Error::Io {
                    operation: "write",
                    code: 1,
                });
            }
            self.bytes.extend_from_slice(bytes);
            Ok(())
        }
    }

    impl PatchSink for FailingSink {
        fn write_at(&mut self, offset: usize, bytes: &[u8]) -> Result<(), Error> {
            self.bytes[offset..offset + bytes.len()].copy_from_slice(bytes);
            Ok(())
        }
    }

    struct Header {
        riff: u32,
        format_size: u32,
        tag: u16,
        channels: u16,
        rate: u32,
        byte_rate: u32,
        block_align: u16,
        bits: u16,
        data: u32,
    }

    fn field<const N: usize>(bytes: &[u8], offset: usize) -> [u8; N] {
        bytes[offset..offset + N].try_into().unwrap()
    }

    fn parse_header(bytes: &[u8]) -> Header {
        assert_eq!(&bytes[..4], b"RIFF", "the file starts with the RIFF id");
        assert_eq!(&bytes[8..12], b"WAVE", "the form type is `WAVE`");
        assert_eq!(&bytes[12..16], b"fmt ", "the first chunk is `fmt `");
        assert_eq!(&bytes[36..40], b"data", "the data chunk follows `fmt `");
        Header {
            riff: u32::from_le_bytes(field(bytes, 4)),
            format_size: u32::from_le_bytes(field(bytes, 16)),
            tag: u16::from_le_bytes(field(bytes, 20)),
            channels: u16::from_le_bytes(field(bytes, 22)),
            rate: u32::from_le_bytes(field(bytes, 24)),
            byte_rate: u32::from_le_bytes(field(bytes, 28)),
            block_align: u16::from_le_bytes(field(bytes, 32)),
            bits: u16::from_le_bytes(field(bytes, 34)),
            data: u32::from_le_bytes(field(bytes, 40)),
        }
    }

    fn decode(bytes: &[u8]) -> Vec<i16> {
        bytes[HEADER_LEN..]
            .as_chunks::<2>()
            .0
            .iter()
            .map(|pair| i16::from_le_bytes(*pair))
            .collect()
    }

    fn spec(channels: Channels) -> RecordingSpec {
        RecordingSpec::new(48_000, channels).unwrap()
    }

    #[test]
    fn the_header_describes_the_pcm_format() {
        for channels in [Channels::Mono, Channels::Stereo] {
            for rate in [8_000, 44_100, 48_000, 384_000] {
                let spec = RecordingSpec::new(rate, channels).unwrap();
                let writer = WavWriter::new(TestSink::default(), spec).unwrap();
                let header = parse_header(&writer.sink.bytes);
                let block_align = channels.count() * SAMPLE_BYTES;
                assert_eq!(writer.sink.bytes.len(), HEADER_LEN);
                assert_eq!(header.format_size, 16, "a PCM `fmt ` chunk is 16 bytes");
                assert_eq!(header.tag, PCM_FORMAT);
                assert_eq!(header.channels, channels.count());
                assert_eq!(header.rate, rate);
                assert_eq!(header.byte_rate, rate * u32::from(block_align));
                assert_eq!(header.block_align, block_align);
                assert_eq!(header.bits, BITS_PER_SAMPLE);
                assert_eq!(header.riff, RIFF_PREFIX);
                assert_eq!(header.data, 0);
            }
        }
    }

    #[test]
    fn samples_are_quantized_to_sixteen_bit_pcm() {
        let tie = 1.5 / PCM_SCALE;
        let cases = [
            (0.0_f32, 0_i16),
            (0.25, 8_192),
            (0.5, 16_384),
            (-0.5, -16_384),
            (1.0, i16::MAX),
            (-1.0, i16::MIN),
            (tie, 2),
            (-tie, -2),
            (f32::NAN, 0),
            (f32::INFINITY, 0),
            (f32::NEG_INFINITY, 0),
            (2.0, i16::MAX),
            (-2.0, i16::MIN),
            (f32::MAX, i16::MAX),
            (f32::MIN, i16::MIN),
        ];
        let samples: Vec<f32> = cases.iter().map(|(sample, _)| *sample).collect();
        let mut writer = WavWriter::new(TestSink::default(), spec(Channels::Mono)).unwrap();
        writer.write(&samples).unwrap();
        let sink = writer.finish().unwrap();
        let decoded = decode(&sink.bytes);
        assert_eq!(decoded.len(), cases.len());
        for (index, ((sample, expected), actual)) in cases.iter().zip(&decoded).enumerate() {
            assert_eq!(
                actual, expected,
                "sample {sample} at {index} must quantize to {expected}"
            );
        }
    }

    #[test]
    fn the_header_reports_the_final_sizes_after_finish() {
        let mut writer = WavWriter::new(TestSink::default(), spec(Channels::Stereo)).unwrap();
        let placeholder = writer.sink.bytes.clone();
        writer.write(&[0.25; 8]).unwrap();
        let sink = writer.finish().unwrap();

        let header = parse_header(&sink.bytes);
        assert_eq!(header.data, 16, "four stereo frames are sixteen bytes");
        assert_eq!(header.riff, 52);
        assert_eq!(sink.bytes.len(), HEADER_LEN + 16);
        assert_eq!(&sink.bytes[..4], &placeholder[..4]);
        assert_eq!(
            &sink.bytes[8..40],
            &placeholder[8..40],
            "only the two size fields change"
        );
    }

    #[test]
    fn finish_rewrites_the_whole_header_at_offset_zero() {
        let mut writer = WavWriter::new(TestSink::default(), spec(Channels::Stereo)).unwrap();
        writer.write(&[0.5; 4]).unwrap();
        let sink = writer.finish().unwrap();

        assert_eq!(sink.patches.len(), 1, "the header is rewritten once");
        let (offset, bytes) = &sink.patches[0];
        assert_eq!(*offset, 0);
        assert_eq!(bytes.len(), HEADER_LEN);
        assert_eq!(bytes.as_slice(), &sink.bytes[..HEADER_LEN]);
    }

    #[test]
    fn every_block_streams_to_the_sink_in_order() {
        let mut writer = WavWriter::new(TestSink::default(), spec(Channels::Stereo)).unwrap();
        writer.write(&[0.25; 4]).unwrap();
        writer.write(&[0.5; 6]).unwrap();
        writer.write(&[-0.5; 2]).unwrap();
        let sink = writer.finish().unwrap();

        assert_eq!(
            sink.appends,
            vec![HEADER_LEN, 8, 12, 4],
            "one append per block, with that block's byte length"
        );
        let mut expected = vec![8_192_i16; 4];
        expected.extend_from_slice(&[16_384; 6]);
        expected.extend_from_slice(&[-16_384; 2]);
        assert_eq!(decode(&sink.bytes), expected);
    }

    #[test]
    fn an_empty_recording_is_a_header_only_file() {
        let writer = WavWriter::new(TestSink::default(), spec(Channels::Stereo)).unwrap();
        let sink = writer.finish().unwrap();

        assert_eq!(sink.bytes.len(), HEADER_LEN);
        assert_eq!(sink.appends, vec![HEADER_LEN]);
        let header = parse_header(&sink.bytes);
        assert_eq!(header.data, 0);
        assert_eq!(header.riff, RIFF_PREFIX);
    }

    #[test]
    fn a_partial_frame_is_rejected() {
        let mut writer = WavWriter::new(TestSink::default(), spec(Channels::Stereo)).unwrap();
        assert_eq!(
            writer.write(&[0.0; 3]),
            Err(Error::MisalignedBuffer {
                len: 3,
                channels: 2
            })
        );
        assert_eq!(writer.sink.bytes.len(), HEADER_LEN);
    }

    #[test]
    fn a_write_past_the_limit_is_refused() {
        let mut writer =
            WavWriter::with_limit(TestSink::default(), spec(Channels::Stereo), 8).unwrap();
        writer.write(&[0.25; 2]).unwrap();
        writer.write(&[0.5; 2]).unwrap();
        assert_eq!(
            writer.write(&[0.75; 2]),
            Err(Error::RecordingTooLarge { bytes: 12, max: 8 })
        );
        assert_eq!(writer.sink.bytes.len(), HEADER_LEN + 8);
    }

    #[test]
    fn a_recording_at_the_limit_still_finishes() {
        let mut writer =
            WavWriter::with_limit(TestSink::default(), spec(Channels::Stereo), 8).unwrap();
        writer.write(&[0.25; 2]).unwrap();
        writer.write(&[0.5; 2]).unwrap();
        assert!(writer.write(&[0.75; 2]).is_err());
        let sink = writer.finish().unwrap();

        let header = parse_header(&sink.bytes);
        assert_eq!(header.data, 8);
        assert_eq!(header.riff, 44);
        assert_eq!(sink.bytes.len(), HEADER_LEN + 8);
    }

    #[test]
    fn the_limit_is_the_riff_field_not_the_data_field() {
        let full = ChunkSizes::EMPTY
            .grow(MAX_DATA_BYTES, MAX_DATA_BYTES)
            .unwrap();
        assert_eq!(full.data, u32::MAX - RIFF_PREFIX);
        assert_eq!(
            full.riff,
            u32::MAX,
            "the RIFF field reaches its ceiling first"
        );
        assert_eq!(MAX_DATA_BYTES, u64::from(u32::MAX) - u64::from(RIFF_PREFIX));
        assert_eq!(
            ChunkSizes::EMPTY.grow(MAX_DATA_BYTES + 1, MAX_DATA_BYTES),
            Err(Error::RecordingTooLarge {
                bytes: MAX_DATA_BYTES + 1,
                max: MAX_DATA_BYTES
            })
        );
        assert_eq!(
            ChunkSizes::EMPTY.grow(MAX_DATA_BYTES + 1, u64::MAX),
            Err(Error::RecordingTooLarge {
                bytes: MAX_DATA_BYTES + 1,
                max: MAX_DATA_BYTES
            }),
            "a limit above the size field is capped at it"
        );
    }

    #[test]
    fn a_block_the_sink_refuses_is_not_counted() {
        let sink = FailingSink {
            bytes: Vec::new(),
            calls: 0,
            accepted: 2,
        };
        let mut writer = WavWriter::new(sink, spec(Channels::Stereo)).unwrap();
        writer.write(&[0.25; 4]).unwrap();
        assert_eq!(
            writer.write(&[0.5; 4]),
            Err(Error::Io {
                operation: "write",
                code: 1
            })
        );
        let sink = writer.finish().unwrap();

        let header = parse_header(&sink.bytes);
        assert_eq!(header.data, 8, "only the accepted block is described");
        assert_eq!(sink.bytes.len(), HEADER_LEN + 8);
    }
}
