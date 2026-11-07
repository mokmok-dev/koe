//! The audio file backends behind a recorder.

use std::fs::File;
use std::io::{BufWriter, Seek, SeekFrom, Write};
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use koe_record_core::{
    Channels, Error, OggOpusConfig, OggOpusWriter, PatchSink, RecordingSpec, Sink, WavWriter,
};
// shiguredo/opus-rs downloads a prebuilt static library in its build script,
// which a sandboxed Nix build cannot do. `opusic-sys` compiles the same libopus
// source with CMake, and the encoder settings and lookahead handling below
// follow the API opus-rs exposes.
use opus::{Application, Bitrate};

use crate::format::Format;

/// Decoded samples one 20 ms packet carries at 48 kHz.
const PACKET_SAMPLES_48K: u32 = 960;

/// Buffer handed to `encode_float`.
const MAX_PACKET_BYTES: usize = 4_096;

/// Bitrate a stereo recording targets, in bits per second.
const STEREO_BITRATE: i32 = 96_000;

/// Bitrate a mono recording targets, in bits per second.
const MONO_BITRATE: i32 = 64_000;

/// A format-specific writer.
pub trait AudioFile: Send {
    /// Appends one block of interleaved samples holding whole frames.
    ///
    /// # Errors
    ///
    /// Returns [`Error::MisalignedBuffer`] for a partial frame,
    /// [`Error::RecordingTooLarge`] when the container's size field can no
    /// longer describe the audio, and the codec's or the file's error when a
    /// block cannot be written.
    fn write(&mut self, samples: &[f32]) -> Result<(), Error>;

    /// Completes the file and releases it.
    ///
    /// # Errors
    ///
    /// Returns the codec's or the file's error when the last block cannot be
    /// written.
    fn finish(self: Box<Self>) -> Result<(), Error>;
}

/// Opens the writer `format` names, creating or truncating `path`.
///
/// # Errors
///
/// Returns the file's or the codec's error when the recording cannot start.
pub fn open(path: &Path, spec: RecordingSpec, format: Format) -> Result<Box<dyn AudioFile>, Error> {
    match format {
        Format::Ogg => Ok(Box::new(OggOpusFile::create(path, spec)?)),
        Format::Wav => Ok(Box::new(wav_file(path, spec)?)),
    }
}

fn wav_file(path: &Path, spec: RecordingSpec) -> Result<WavWriter<FileSink>, Error> {
    let file = File::create(path).map_err(io_error("create"))?;
    WavWriter::new(FileSink::new(file), spec)
}

/// An Ogg Opus file fed with whole-frame blocks at the recording rate.
struct OggOpusFile {
    encoder: opus::Encoder,
    writer: OggOpusWriter<FileSink>,
    channels: usize,
    sample_rate: u32,
    frame: Vec<f32>,
    pending: Vec<f32>,
    packet: Vec<u8>,
    pre_skip_48k: u64,
    input_frames: u64,
    encoded_frames: u64,
}

impl OggOpusFile {
    fn create(path: &Path, spec: RecordingSpec) -> Result<Self, Error> {
        let channels = spec.channels();
        let sample_rate = spec.sample_rate();
        let mut encoder =
            opus::Encoder::new(sample_rate, opus_channels(channels), Application::Audio)
                .map_err(encode_error("create"))?;
        let bitrate = match channels {
            Channels::Stereo => STEREO_BITRATE,
            Channels::Mono => MONO_BITRATE,
        };
        encoder
            .set_bitrate(Bitrate::Bits(bitrate))
            .map_err(encode_error("set bitrate"))?;

        // The lookahead is the encoder delay the decoder must drop. Both the
        // OpusHead pre-skip field and the granules count 48 kHz samples, whatever
        // rate the encoder was opened at.
        let lookahead = u32::try_from(encoder.get_lookahead().map_err(encode_error("lookahead"))?)
            .map_err(|_| Error::ContainerInvariant {
                operation: "lookahead",
            })?;
        let pre_skip_48k = u64::from(lookahead) * 48_000 / u64::from(sample_rate);
        let pre_skip = u16::try_from(pre_skip_48k).map_err(|_| Error::ContainerInvariant {
            operation: "pre-skip",
        })?;

        let file = File::create(path).map_err(io_error("create"))?;
        let writer = OggOpusWriter::new(
            FileSink::new(file),
            OggOpusConfig {
                channels,
                pre_skip,
                input_sample_rate: sample_rate,
                serial: serial(),
            },
        )?;

        let channel_count = usize::from(channels.count());
        let frame_samples =
            usize::try_from(sample_rate / 50).map_err(|_| Error::ContainerInvariant {
                operation: "frame size",
            })?;
        Ok(Self {
            encoder,
            writer,
            channels: channel_count,
            sample_rate,
            frame: vec![0.0; frame_samples * channel_count],
            pending: Vec::with_capacity(frame_samples * channel_count * 2),
            packet: vec![0; MAX_PACKET_BYTES],
            pre_skip_48k,
            input_frames: 0,
            encoded_frames: 0,
        })
    }

    /// Encodes the frame in `self.frame`.
    fn encode_frame(&mut self) -> Result<(), Error> {
        let length = self
            .encoder
            .encode_float(&self.frame, &mut self.packet)
            .map_err(encode_error("encode"))?;
        self.writer
            .push_packet(&self.packet[..length], PACKET_SAMPLES_48K)?;
        self.encoded_frames =
            self.encoded_frames
                .checked_add(1)
                .ok_or(Error::ContainerInvariant {
                    operation: "encode frame",
                })?;
        Ok(())
    }
}

impl AudioFile for OggOpusFile {
    fn write(&mut self, samples: &[f32]) -> Result<(), Error> {
        if !samples.len().is_multiple_of(self.channels) {
            return Err(Error::MisalignedBuffer {
                len: samples.len(),
                channels: self.channels,
            });
        }
        let frames = u64::try_from(samples.len() / self.channels).map_err(|_| {
            Error::ContainerInvariant {
                operation: "write block",
            }
        })?;
        self.input_frames =
            self.input_frames
                .checked_add(frames)
                .ok_or(Error::ContainerInvariant {
                    operation: "write block",
                })?;

        self.pending.extend_from_slice(samples);
        let frame_len = self.frame.len();
        while self.pending.len() >= frame_len {
            self.frame.copy_from_slice(&self.pending[..frame_len]);
            self.pending.drain(..frame_len);
            self.encode_frame()?;
        }
        Ok(())
    }

    fn finish(mut self: Box<Self>) -> Result<(), Error> {
        let scaled = self
            .input_frames
            .checked_mul(48_000)
            .ok_or(Error::ContainerInvariant {
                operation: "finish",
            })?
            / u64::from(self.sample_rate);
        let final_granule =
            self.pre_skip_48k
                .checked_add(scaled)
                .ok_or(Error::ContainerInvariant {
                    operation: "finish",
                })?;

        // The last partial frame goes out padded with silence; the final granule
        // trims that padding, which RFC 7845 section 4.4 expects.
        if !self.pending.is_empty() || self.encoded_frames == 0 {
            let frame_len = self.frame.len();
            self.pending.resize(frame_len, 0.0);
            self.frame.copy_from_slice(&self.pending[..frame_len]);
            self.pending.clear();
            self.encode_frame()?;
        }
        if !self.pending.is_empty() {
            return Err(Error::ContainerInvariant {
                operation: "finish",
            });
        }

        // A granule that needs more padding than the lookahead accounts for
        // means the packet count and the duration disagree. The upper bound
        // also stops a bad granule from making this loop run for hours.
        let target_packets = final_granule.div_ceil(u64::from(PACKET_SAMPLES_48K));
        let padding = self.pre_skip_48k.div_ceil(u64::from(PACKET_SAMPLES_48K)) + 2;
        if target_packets > self.encoded_frames.saturating_add(padding) {
            return Err(Error::ContainerInvariant {
                operation: "finish",
            });
        }
        while self.encoded_frames < target_packets {
            let before = self.encoded_frames;
            self.frame.fill(0.0);
            self.encode_frame()?;
            if self.encoded_frames == before {
                return Err(Error::ContainerInvariant {
                    operation: "encode frame",
                });
            }
        }

        let Self { writer, .. } = *self;
        writer.finish(final_granule)?.finish()
    }
}

impl AudioFile for WavWriter<FileSink> {
    fn write(&mut self, samples: &[f32]) -> Result<(), Error> {
        Self::write(self, samples)
    }

    fn finish(self: Box<Self>) -> Result<(), Error> {
        Self::finish(*self)?.finish()
    }
}

/// Where encoded bytes are written.
struct FileSink {
    writer: BufWriter<File>,
}

impl FileSink {
    fn new(file: File) -> Self {
        Self {
            writer: BufWriter::new(file),
        }
    }

    fn finish(self) -> Result<(), Error> {
        self.writer.into_inner().map_err(|error| {
            let code = error.into_error();
            Error::Io {
                operation: "flush",
                code: code.raw_os_error().unwrap_or(0),
            }
        })?;
        Ok(())
    }
}

impl Sink for FileSink {
    fn write_all(&mut self, bytes: &[u8]) -> Result<(), Error> {
        self.writer.write_all(bytes).map_err(io_error("write"))
    }
}

impl PatchSink for FileSink {
    fn write_at(&mut self, offset: usize, bytes: &[u8]) -> Result<(), Error> {
        let offset =
            u64::try_from(offset).map_err(|_| Error::ContainerInvariant { operation: "seek" })?;
        // `Seek` on a `BufWriter` flushes first; seeking the inner file would
        // strand buffered audio behind the header.
        self.writer
            .seek(SeekFrom::Start(offset))
            .map_err(io_error("seek"))?;
        self.writer.write_all(bytes).map_err(io_error("write"))?;
        // The append position stays at the end, so a later block appends.
        self.writer
            .seek(SeekFrom::End(0))
            .map_err(io_error("seek"))?;
        Ok(())
    }
}

fn encode_error(operation: &'static str) -> impl Fn(opus::Error) -> Error {
    move |error| Error::Encode {
        operation,
        code: error.code() as i32,
    }
}

fn io_error(operation: &'static str) -> impl Fn(std::io::Error) -> Error {
    move |error| Error::Io {
        operation,
        code: error.raw_os_error().unwrap_or(0),
    }
}

const fn opus_channels(channels: Channels) -> opus::Channels {
    match channels {
        Channels::Mono => opus::Channels::Mono,
        Channels::Stereo => opus::Channels::Stereo,
    }
}

/// Derives a page serial from the clock so two recordings never share one.
fn serial() -> u32 {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |value| value.as_nanos());
    serial_at(nanos)
}

/// Keeps the low 32 bits of `nanos` and forces the result odd.
fn serial_at(nanos: u128) -> u32 {
    u32::try_from(nanos & u128::from(u32::MAX)).unwrap_or(1) | 1
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::float_cmp)]

    use std::collections::HashSet;

    use super::*;

    struct Page {
        header_type: u8,
        granule: u64,
        packets: Vec<Vec<u8>>,
    }

    /// Parses the page headers and packet payloads the writer produced.
    fn pages(bytes: &[u8]) -> Vec<Page> {
        let mut pages = Vec::new();
        let mut offset = 0;
        while offset < bytes.len() {
            assert_eq!(&bytes[offset..offset + 4], b"OggS");
            let header_type = bytes[offset + 5];
            let granule = u64::from_le_bytes(bytes[offset + 6..offset + 14].try_into().unwrap());
            let segments = usize::from(bytes[offset + 26]);
            let lacing = &bytes[offset + 27..offset + 27 + segments];
            let body_start = offset + 27 + segments;
            let body_len: usize = lacing.iter().map(|value| usize::from(*value)).sum();
            let body = &bytes[body_start..body_start + body_len];

            let mut packets = Vec::new();
            let mut start = 0;
            let mut length = 0;
            for value in lacing {
                length += usize::from(*value);
                if *value < 255 {
                    packets.push(body[start..start + length].to_vec());
                    start += length;
                    length = 0;
                }
            }
            pages.push(Page {
                header_type,
                granule,
                packets,
            });
            offset = body_start + body_len;
        }
        pages
    }

    fn audio_packets(pages: &[Page]) -> usize {
        pages.iter().skip(2).map(|page| page.packets.len()).sum()
    }

    fn create(
        dir: &tempfile::TempDir,
        name: &str,
        spec: RecordingSpec,
    ) -> (std::path::PathBuf, OggOpusFile) {
        let path = dir.path().join(name);
        (path.clone(), OggOpusFile::create(&path, spec).unwrap())
    }

    #[test]
    fn the_opus_head_uses_the_encoder_lookahead_as_pre_skip() {
        let dir = tempfile::tempdir().unwrap();
        let spec = RecordingSpec::new(48_000, Channels::Stereo).unwrap();
        let (path, file) = create(&dir, "head.ogg", spec);
        Box::new(file).finish().unwrap();

        let bytes = std::fs::read(path).unwrap();
        let pages = pages(&bytes);
        let head = &pages[0].packets[0];
        assert_eq!(&head[..8], b"OpusHead");
        let pre_skip = u16::from_le_bytes(head[10..12].try_into().unwrap());

        let mut encoder =
            opus::Encoder::new(48_000, opus::Channels::Stereo, Application::Audio).unwrap();
        let expected = u16::try_from(encoder.get_lookahead().unwrap()).unwrap();
        assert_eq!(pre_skip, expected);
    }

    #[test]
    fn a_partial_frame_is_encoded_and_trimmed_by_the_final_granule() {
        let dir = tempfile::tempdir().unwrap();
        let spec = RecordingSpec::new(48_000, Channels::Mono).unwrap();
        let (path, mut file) = create(&dir, "partial.ogg", spec);
        file.write(&[0.25; 100]).unwrap();
        let pre_skip = file.pre_skip_48k;
        Box::new(file).finish().unwrap();

        let pages = pages(&std::fs::read(path).unwrap());
        let eos = pages.last().unwrap();
        assert_ne!(eos.header_type & 0x04, 0);
        assert_eq!(eos.granule, pre_skip + 100);
    }

    #[test]
    fn exact_packets_do_not_gain_an_extra_padding_packet() {
        let dir = tempfile::tempdir().unwrap();
        let spec = RecordingSpec::new(48_000, Channels::Mono).unwrap();
        let (path, mut file) = create(&dir, "exact.ogg", spec);
        let samples = vec![0.0; usize::try_from(PACKET_SAMPLES_48K).unwrap()];
        file.write(&samples).unwrap();
        let pre_skip = file.pre_skip_48k;
        Box::new(file).finish().unwrap();

        let pages = pages(&std::fs::read(path).unwrap());
        assert_eq!(audio_packets(&pages), 2);
        assert_eq!(pages.last().unwrap().granule, pre_skip + 960);
    }

    #[test]
    fn a_partial_frame_after_a_full_packet_is_written() {
        let dir = tempfile::tempdir().unwrap();
        let spec = RecordingSpec::new(48_000, Channels::Mono).unwrap();
        let (path, mut file) = create(&dir, "tail.ogg", spec);
        // One full packet first, so the partial frame is the second packet.
        file.write(&vec![0.5; 1060]).unwrap();
        let pre_skip = file.pre_skip_48k;
        Box::new(file).finish().unwrap();

        let pages = pages(&std::fs::read(path).unwrap());
        assert_eq!(pages.last().unwrap().granule, pre_skip + 1060);
    }

    #[test]
    fn finish_accepts_padding_at_the_guard_boundary() {
        let dir = tempfile::tempdir().unwrap();
        let spec = RecordingSpec::new(48_000, Channels::Mono).unwrap();
        let (path, mut file) = create(&dir, "boundary.ogg", spec);
        // The packet count is behind the duration; the guard should still
        // accept the padding this needs and stop there.
        file.input_frames = 2_880;
        file.encoded_frames = 0;
        file.pending = vec![0.0];
        Box::new(file).finish().unwrap();
        let pages = pages(&std::fs::read(path).unwrap());
        assert_ne!(pages.last().unwrap().header_type & 0x04, 0);
    }

    #[test]
    fn an_empty_take_still_writes_one_packet() {
        let dir = tempfile::tempdir().unwrap();
        let spec = RecordingSpec::new(48_000, Channels::Mono).unwrap();
        let (path, file) = create(&dir, "empty.ogg", spec);
        let pre_skip = file.pre_skip_48k;
        Box::new(file).finish().unwrap();

        let pages = pages(&std::fs::read(path).unwrap());
        assert_eq!(audio_packets(&pages), 1);
        assert_eq!(pages.last().unwrap().granule, pre_skip);
    }

    #[test]
    fn serials_keep_the_low_bits_and_stay_odd() {
        assert_eq!(serial_at(0x1_0000_0002), 3);
        assert_eq!(serial_at(2), 3);
        assert_eq!(serial_at(3), 3);
        assert_eq!(serial_at(0), 1);
        assert_eq!(serial_at(u128::MAX), u32::MAX);
    }

    #[test]
    fn serials_vary_between_calls() {
        let serials: HashSet<u32> = (0..256).map(|_| serial()).collect();
        assert!(
            serials.len() > 1,
            "the clock must produce different serials"
        );
        assert!(serials.iter().all(|serial| serial & 1 == 1));
    }

    #[test]
    fn a_failed_flush_is_reported() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("readonly");
        std::fs::write(&path, b"x").unwrap();
        let mut sink = FileSink::new(File::open(path).unwrap());
        sink.write_all(b"buffered").unwrap();
        assert!(sink.finish().is_err());
    }
}
