//! End-to-end coverage for single-file recording.

#![allow(
    clippy::unwrap_used,
    clippy::cast_precision_loss,
    clippy::cast_possible_truncation,
    clippy::float_cmp
)]

use std::f64::consts::TAU;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use koe_record::{Channels, Error, Recorder, RecordingSpec, Source, StreamSpec};

const RATE: u32 = 48_000;
const BLOCK_FRAMES: usize = 960;
const TOTAL_FRAMES: usize = RATE as usize;

struct TempFile {
    path: PathBuf,
}

impl TempFile {
    fn new(extension: &str) -> Self {
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|value| value.as_nanos())
            .unwrap_or_default();
        let serial = COUNTER.fetch_add(1, Ordering::Relaxed);
        let name = format!(
            "koe-record-{}-{nanos}-{serial}.{extension}",
            std::process::id()
        );
        Self {
            path: std::env::temp_dir().join(name),
        }
    }

    fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for TempFile {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

struct Page {
    header_type: u8,
    granule: i64,
    packets: Vec<Vec<u8>>,
}

struct Header {
    channels: u8,
    pre_skip: u16,
    input_rate: u32,
}

struct WavHeader {
    riff_len: u32,
    data_len: u32,
    channels: u16,
    sample_rate: u32,
    byte_rate: u32,
    block_align: u16,
    bits: u16,
}

fn parse_crc(bytes: &[u8]) -> u32 {
    let mut crc = 0_u32;
    for byte in bytes {
        crc ^= u32::from(*byte) << 24;
        for _ in 0..8 {
            crc = if crc & 0x8000_0000 == 0 {
                crc << 1
            } else {
                (crc << 1) ^ 0x04C1_1DB7
            };
        }
    }
    crc
}

fn parse_pages(bytes: &[u8]) -> Vec<Page> {
    let mut pages = Vec::new();
    let mut offset = 0;
    while offset < bytes.len() {
        assert_eq!(
            &bytes[offset..offset + 4],
            b"OggS",
            "every page starts with the capture pattern"
        );
        assert_eq!(bytes[offset + 4], 0, "the page version is zero");
        let header_type = bytes[offset + 5];
        let granule = i64::from_le_bytes(bytes[offset + 6..offset + 14].try_into().unwrap());
        let crc = u32::from_le_bytes(bytes[offset + 22..offset + 26].try_into().unwrap());
        let segments = usize::from(bytes[offset + 26]);
        let lacing_start = offset + 27;
        let lacing = &bytes[lacing_start..lacing_start + segments];
        let body_len: usize = lacing.iter().map(|value| usize::from(*value)).sum();
        let body_start = lacing_start + segments;
        let body = &bytes[body_start..body_start + body_len];
        let end = body_start + body_len;

        let mut zeroed = bytes[offset..end].to_vec();
        zeroed[22..26].fill(0);
        assert_eq!(crc, parse_crc(&zeroed), "page CRC covers the whole page");

        let mut packets = Vec::new();
        let mut start = 0;
        let mut length = 0;
        for value in lacing {
            length += usize::from(*value);
            if *value != 255 {
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
        offset = end;
    }
    pages
}

fn parse_head(packet: &[u8]) -> Header {
    assert_eq!(&packet[..8], b"OpusHead");
    assert_eq!(packet[8], 1, "the version is 1");
    Header {
        channels: packet[9],
        pre_skip: u16::from_le_bytes([packet[10], packet[11]]),
        input_rate: u32::from_le_bytes(packet[12..16].try_into().unwrap()),
    }
}

fn decode(pages: &[Page]) -> Vec<f32> {
    let mut decoder = opus::Decoder::new(RATE, opus::Channels::Stereo).unwrap();
    let mut samples = vec![0.0_f32; 5_760 * 2];
    let mut samples_out = Vec::new();
    for page in pages {
        for packet in &page.packets {
            let frames = decoder.decode_float(packet, &mut samples, false).unwrap();
            samples_out.extend_from_slice(&samples[..frames * 2]);
        }
    }
    samples_out
}

/// Parses a canonical 16-bit PCM WAV, asserting the chunks a reader needs.
fn parse_wav(bytes: &[u8]) -> (WavHeader, Vec<i16>) {
    assert_eq!(
        bytes.get(..4),
        Some(&b"RIFF"[..]),
        "the file starts with the RIFF id"
    );
    assert_eq!(
        bytes.get(8..12),
        Some(&b"WAVE"[..]),
        "the form type is WAVE"
    );
    assert_eq!(
        bytes.get(12..16),
        Some(&b"fmt "[..]),
        "the first chunk is `fmt `"
    );
    let format_size = u32::from_le_bytes(bytes[16..20].try_into().unwrap());
    assert_eq!(format_size, 16, "a PCM `fmt ` chunk is 16 bytes");
    let tag = u16::from_le_bytes(bytes[20..22].try_into().unwrap());
    assert_eq!(tag, 1, "the format tag is PCM");
    assert_eq!(&bytes[36..40], b"data", "the data chunk follows `fmt `");

    let riff_len = u32::from_le_bytes(bytes[4..8].try_into().unwrap());
    let data_len = u32::from_le_bytes(bytes[40..44].try_into().unwrap());
    let end = 44 + usize::try_from(data_len).unwrap();
    assert!(end <= bytes.len(), "the data chunk lies inside the file");
    let samples = bytes[44..end]
        .as_chunks::<2>()
        .0
        .iter()
        .map(|pair| i16::from_le_bytes(*pair))
        .collect();
    (
        WavHeader {
            riff_len,
            data_len,
            channels: u16::from_le_bytes(bytes[22..24].try_into().unwrap()),
            sample_rate: u32::from_le_bytes(bytes[24..28].try_into().unwrap()),
            byte_rate: u32::from_le_bytes(bytes[28..32].try_into().unwrap()),
            block_align: u16::from_le_bytes(bytes[32..34].try_into().unwrap()),
            bits: u16::from_le_bytes(bytes[34..36].try_into().unwrap()),
        },
        samples,
    )
}

fn correlation(actual: &[f32], expected: &[f32]) -> f64 {
    let len = actual.len().min(expected.len());
    let count = len as f64;
    let (mut sum_actual, mut sum_expected, mut sum_actual_squared, mut sum_expected_squared) =
        (0.0, 0.0, 0.0, 0.0);
    let mut sum_product = 0.0;
    for index in 0..len {
        let actual = f64::from(actual[index]);
        let expected = f64::from(expected[index]);
        sum_actual += actual;
        sum_expected += expected;
        sum_actual_squared = actual.mul_add(actual, sum_actual_squared);
        sum_expected_squared = expected.mul_add(expected, sum_expected_squared);
        sum_product = actual.mul_add(expected, sum_product);
    }
    let covariance = sum_product - sum_actual * sum_expected / count;
    let actual_variance = sum_actual_squared - sum_actual * sum_actual / count;
    let expected_variance = sum_expected_squared - sum_expected * sum_expected / count;
    covariance / (actual_variance * expected_variance).sqrt()
}

fn recording_spec() -> RecordingSpec {
    RecordingSpec::new(RATE, Channels::Stereo).unwrap()
}

fn stream_spec(source: Source, channels: u16) -> StreamSpec {
    StreamSpec::new(source, RATE, channels).unwrap()
}

fn stereo_recorder(path: &Path) -> Recorder {
    let mut recorder = Recorder::create(path, recording_spec()).unwrap();
    recorder
        .add_source(Source::Microphone, &stream_spec(Source::Microphone, 1))
        .unwrap();
    recorder
        .add_source(Source::SystemAudio, &stream_spec(Source::SystemAudio, 2))
        .unwrap();
    recorder
}

#[test]
fn records_a_mix_that_decodes_to_the_recorded_length() {
    let file = TempFile::new("ogg");
    let mut recorder = stereo_recorder(file.path());

    let mut microphone = vec![0.0_f32; BLOCK_FRAMES];
    let mut system_audio = vec![0.0_f32; BLOCK_FRAMES * 2];
    let mut expected = Vec::with_capacity(TOTAL_FRAMES * 2);
    for block in 0..TOTAL_FRAMES / BLOCK_FRAMES {
        for frame in 0..BLOCK_FRAMES {
            let index = block * BLOCK_FRAMES + frame;
            let time = index as f64 / f64::from(RATE);
            let microphone_sample = (0.2 * f64::sin(TAU * 440.0 * time)) as f32;
            let left = (0.3 * f64::sin(TAU * 880.0 * time)) as f32;
            let right = (0.3 * f64::sin(TAU * 1_320.0 * time)) as f32;
            microphone[frame] = microphone_sample;
            system_audio[frame * 2] = left;
            system_audio[frame * 2 + 1] = right;
            expected.push(microphone_sample + left);
            expected.push(microphone_sample + right);
        }
        recorder.write(Source::Microphone, &microphone).unwrap();
        recorder.write(Source::SystemAudio, &system_audio).unwrap();
    }

    let summary = recorder.finish().unwrap();
    assert_eq!(summary.frames, TOTAL_FRAMES as u64);
    assert_eq!(summary.duration, Duration::from_secs(1));
    assert_eq!(summary.clipped_samples, 0);

    let bytes = std::fs::read(file.path()).unwrap();
    let pages = parse_pages(&bytes);
    assert!(pages.len() > 3, "the stream spans several pages");
    let header = parse_head(&pages[0].packets[0]);
    assert_eq!(
        pages[0].header_type, 0x02,
        "the first page starts the stream"
    );
    assert_eq!(header.channels, 2);
    assert_eq!(header.input_rate, RATE);
    assert_eq!(&pages[1].packets[0][..8], b"OpusTags");

    let final_granule = pages.last().unwrap().granule;
    assert_eq!(
        final_granule - i64::from(header.pre_skip),
        i64::try_from(TOTAL_FRAMES).unwrap(),
        "the final granule minus the pre-skip is the recorded length"
    );

    let decoded = decode(&pages[2..]);
    let decoded_frames = i64::try_from(decoded.len() / 2).unwrap();
    assert!(
        decoded_frames >= final_granule,
        "the decoder must deliver every sample the granule counts"
    );
    assert!(
        decoded_frames - final_granule < i64::from(960),
        "the padding after the final granule stays under one packet"
    );

    let skip = usize::from(header.pre_skip);
    assert!(decoded.len() / 2 >= skip + TOTAL_FRAMES);
    let played = &decoded[skip * 2..(skip + TOTAL_FRAMES) * 2];
    let left: Vec<f32> = played
        .as_chunks::<2>()
        .0
        .iter()
        .map(|frame| frame[0])
        .collect();
    let right: Vec<f32> = played
        .as_chunks::<2>()
        .0
        .iter()
        .map(|frame| frame[1])
        .collect();
    let expected_left: Vec<f32> = expected
        .as_chunks::<2>()
        .0
        .iter()
        .map(|frame| frame[0])
        .collect();
    let expected_right: Vec<f32> = expected
        .as_chunks::<2>()
        .0
        .iter()
        .map(|frame| frame[1])
        .collect();
    assert!(
        correlation(&left, &expected_left) > 0.9,
        "the left channel must follow the microphone plus the left system tone"
    );
    assert!(
        correlation(&right, &expected_right) > 0.9,
        "the right channel must follow the microphone plus the right system tone"
    );
}

#[test]
fn an_unknown_extension_is_rejected() {
    let file = TempFile::new("mp3");
    let error = Recorder::create(file.path(), recording_spec())
        .err()
        .unwrap();
    assert_eq!(
        error,
        Error::UnknownFormat {
            extension: "mp3".into()
        }
    );
    assert!(
        !file.path().exists(),
        "no file is created for an unknown format"
    );
}

#[test]
fn a_path_without_an_extension_is_rejected() {
    let error = Recorder::create(
        std::env::temp_dir().join("koe-record-extensionless"),
        recording_spec(),
    )
    .err()
    .unwrap();
    assert_eq!(
        error,
        Error::UnknownFormat {
            extension: "".into()
        }
    );
}

#[test]
fn a_rate_the_format_cannot_carry_is_rejected() {
    let file = TempFile::new("ogg");
    let spec = RecordingSpec::new(44_100, Channels::Stereo).unwrap();
    let error = Recorder::create(file.path(), spec).err().unwrap();
    assert_eq!(error, Error::UnsupportedRate { rate: 44_100 });
}

#[test]
fn a_stream_rate_that_differs_is_rejected() {
    let file = TempFile::new("ogg");
    let mut recorder = Recorder::create(file.path(), recording_spec()).unwrap();
    let spec = StreamSpec::new(Source::Microphone, 24_000, 1).unwrap();
    let error = recorder
        .add_source(Source::Microphone, &spec)
        .err()
        .unwrap();
    assert_eq!(
        error,
        Error::RateMismatch {
            expected: 48_000,
            actual: 24_000,
        }
    );
}

#[test]
fn more_than_two_channels_is_rejected() {
    let file = TempFile::new("ogg");
    let mut recorder = Recorder::create(file.path(), recording_spec()).unwrap();
    let spec = StreamSpec::new(Source::SystemAudio, RATE, 6).unwrap();
    let error = recorder
        .add_source(Source::SystemAudio, &spec)
        .err()
        .unwrap();
    assert_eq!(
        error,
        Error::UnsupportedChannels {
            source: Source::SystemAudio,
            channels: 6,
        }
    );
}

#[test]
fn an_overflowing_write_is_rejected_before_anything_is_copied() {
    let file = TempFile::new("ogg");
    let mut recorder = stereo_recorder(file.path());
    let second = vec![0.0_f32; TOTAL_FRAMES * 2];
    recorder.write(Source::SystemAudio, &second).unwrap();
    assert_eq!(
        recorder
            .write(Source::SystemAudio, &[0.0; 4])
            .err()
            .unwrap(),
        Error::InputOverflow(Source::SystemAudio)
    );
}

#[test]
fn a_second_stream_for_the_same_source_is_rejected() {
    let file = TempFile::new("ogg");
    let mut recorder = Recorder::create(file.path(), recording_spec()).unwrap();
    recorder
        .add_source(Source::Microphone, &stream_spec(Source::Microphone, 1))
        .unwrap();
    recorder
        .add_source(Source::Microphone, &stream_spec(Source::Microphone, 1))
        .unwrap();
    let error = recorder
        .add_source(Source::Microphone, &stream_spec(Source::Microphone, 2))
        .err()
        .unwrap();
    assert_eq!(error, Error::SourceAlreadyRegistered(Source::Microphone));
}

#[test]
fn an_unregistered_source_cannot_be_written_or_ended() {
    let file = TempFile::new("ogg");
    let mut recorder = Recorder::create(file.path(), recording_spec()).unwrap();
    assert_eq!(
        recorder
            .write(Source::Microphone, &[0.0, 0.0])
            .err()
            .unwrap(),
        Error::SourceNotRegistered(Source::Microphone)
    );
    assert_eq!(
        recorder.end(Source::Microphone).err().unwrap(),
        Error::SourceNotRegistered(Source::Microphone)
    );
}

#[test]
fn an_ended_source_is_idempotent_and_refuses_writes() {
    let file = TempFile::new("ogg");
    let mut recorder = stereo_recorder(file.path());
    recorder.end(Source::Microphone).unwrap();
    recorder.end(Source::Microphone).unwrap();
    assert_eq!(
        recorder
            .write(Source::Microphone, &[0.0, 0.0])
            .err()
            .unwrap(),
        Error::SourceEnded(Source::Microphone)
    );

    recorder
        .write(Source::SystemAudio, &[0.25; BLOCK_FRAMES * 2])
        .unwrap();
    let summary = recorder.finish().unwrap();
    assert_eq!(
        summary.frames, BLOCK_FRAMES as u64,
        "the live source keeps recording alone"
    );
}

#[test]
fn a_resumed_source_is_rearmed() {
    let file = TempFile::new("ogg");
    let mut recorder = stereo_recorder(file.path());
    recorder.end(Source::Microphone).unwrap();
    recorder
        .add_source(Source::Microphone, &stream_spec(Source::Microphone, 1))
        .unwrap();
    recorder
        .write(Source::Microphone, &[0.0; BLOCK_FRAMES])
        .unwrap();
    recorder
        .write(Source::SystemAudio, &[0.0; BLOCK_FRAMES * 2])
        .unwrap();
    assert_eq!(recorder.finish().unwrap().frames, BLOCK_FRAMES as u64);
}

#[test]
fn a_partial_frame_is_rejected() {
    let file = TempFile::new("ogg");
    let mut recorder = stereo_recorder(file.path());
    assert_eq!(
        recorder
            .write(Source::SystemAudio, &[0.0; 3])
            .err()
            .unwrap(),
        Error::MisalignedBuffer {
            len: 3,
            channels: 2
        }
    );
}

#[test]
fn an_empty_recording_still_carries_audio() {
    let file = TempFile::new("ogg");
    let summary = stereo_recorder(file.path()).finish().unwrap();
    assert_eq!(summary.frames, 0, "an empty recording mixes no frame");
    assert_eq!(summary.clipped_samples, 0);
    assert_eq!(summary.duration, Duration::ZERO);

    let bytes = std::fs::read(file.path()).unwrap();
    let pages = parse_pages(&bytes);
    let header = parse_head(&pages[0].packets[0]);
    let final_granule = pages.last().unwrap().granule;
    assert_eq!(
        final_granule,
        i64::from(header.pre_skip),
        "with no input the granule covers only the pre-skip"
    );
    assert!(
        decode(&pages[2..]).len() >= usize::from(header.pre_skip) * 2,
        "the stream still holds one silent frame"
    );
}

#[test]
fn clipped_samples_are_counted() {
    let file = TempFile::new("ogg");
    let mut recorder = stereo_recorder(file.path());
    recorder
        .write(Source::Microphone, &[0.9; BLOCK_FRAMES])
        .unwrap();
    recorder
        .write(Source::SystemAudio, &[0.9; BLOCK_FRAMES * 2])
        .unwrap();
    let summary = recorder.finish().unwrap();
    assert_eq!(summary.frames, BLOCK_FRAMES as u64);
    assert_eq!(
        summary.clipped_samples,
        (BLOCK_FRAMES * 2) as u64,
        "both output channels of every frame saturate"
    );
}

#[test]
fn a_mono_recording_accepts_a_stereo_stream() {
    let file = TempFile::new("ogg");
    let spec = RecordingSpec::new(RATE, Channels::Mono).unwrap();
    let mut recorder = Recorder::create(file.path(), spec).unwrap();
    recorder
        .add_source(Source::Microphone, &stream_spec(Source::Microphone, 1))
        .unwrap();
    recorder
        .add_source(Source::SystemAudio, &stream_spec(Source::SystemAudio, 2))
        .unwrap();
    recorder
        .write(Source::Microphone, &[0.25; BLOCK_FRAMES])
        .unwrap();
    recorder
        .write(Source::SystemAudio, &[0.5; BLOCK_FRAMES * 2])
        .unwrap();
    assert_eq!(recorder.finish().unwrap().frames, BLOCK_FRAMES as u64);

    let bytes = std::fs::read(file.path()).unwrap();
    let pages = parse_pages(&bytes);
    assert_eq!(parse_head(&pages[0].packets[0]).channels, 1);
}

#[test]
fn a_lower_rate_recording_scales_its_granule() {
    let file = TempFile::new("ogg");
    let rate = 24_000;
    let spec = RecordingSpec::new(rate, Channels::Stereo).unwrap();
    let mut recorder = Recorder::create(file.path(), spec).unwrap();
    recorder
        .add_source(
            Source::Microphone,
            &StreamSpec::new(Source::Microphone, rate, 1).unwrap(),
        )
        .unwrap();
    recorder
        .add_source(
            Source::SystemAudio,
            &StreamSpec::new(Source::SystemAudio, rate, 2).unwrap(),
        )
        .unwrap();

    let frames = 12_000;
    for _ in 0..frames / 240 {
        recorder.write(Source::Microphone, &[0.0; 240]).unwrap();
        recorder.write(Source::SystemAudio, &[0.0; 480]).unwrap();
    }
    let summary = recorder.finish().unwrap();
    assert_eq!(summary.frames, u64::from(rate) / 2);
    assert_eq!(summary.duration, Duration::from_millis(500));

    let bytes = std::fs::read(file.path()).unwrap();
    let pages = parse_pages(&bytes);
    let header = parse_head(&pages[0].packets[0]);
    assert_eq!(header.input_rate, rate);
    let final_granule = pages.last().unwrap().granule;
    assert_eq!(
        final_granule - i64::from(header.pre_skip),
        i64::from(rate),
        "half a second is 24,000 samples at the 48 kHz granule rate"
    );
    assert!(
        i64::try_from(decode(&pages[2..]).len() / 2).unwrap() >= final_granule,
        "the decoder delivers every sample the granule counts"
    );
}

#[test]
fn blocks_of_different_sizes_pair_up() {
    let file = TempFile::new("ogg");
    let mut recorder = stereo_recorder(file.path());
    for _ in 0..3 {
        recorder.write(Source::Microphone, &[0.5; 250]).unwrap();
        recorder.write(Source::Microphone, &[0.5; 250]).unwrap();
        recorder.write(Source::SystemAudio, &[0.25; 1_000]).unwrap();
    }
    let summary = recorder.finish().unwrap();
    assert_eq!(
        summary.frames, 1_500,
        "every frame both sources delivered is mixed once"
    );

    let pages = parse_pages(&std::fs::read(file.path()).unwrap());
    let header = parse_head(&pages[0].packets[0]);
    assert_eq!(
        pages.last().unwrap().granule - i64::from(header.pre_skip),
        1_500
    );
}

#[test]
fn the_recorder_can_cross_a_thread_boundary() {
    fn assert_send<T: Send>() {}
    assert_send::<Recorder>();
}

#[test]
fn records_a_wav_that_parses_to_the_mixed_samples() {
    let file = TempFile::new("wav");
    let rate = 44_100;
    let spec = RecordingSpec::new(rate, Channels::Stereo).unwrap();
    let mut recorder = Recorder::create(file.path(), spec).unwrap();
    recorder
        .add_source(
            Source::Microphone,
            &StreamSpec::new(Source::Microphone, rate, 1).unwrap(),
        )
        .unwrap();
    recorder
        .add_source(
            Source::SystemAudio,
            &StreamSpec::new(Source::SystemAudio, rate, 2).unwrap(),
        )
        .unwrap();

    let mut system_audio = vec![0.0_f32; BLOCK_FRAMES * 2];
    for frame in 0..BLOCK_FRAMES {
        system_audio[frame * 2] = 0.5;
        system_audio[frame * 2 + 1] = -0.5;
    }
    recorder
        .write(Source::Microphone, &[0.25; BLOCK_FRAMES])
        .unwrap();
    recorder.write(Source::SystemAudio, &system_audio).unwrap();

    let summary = recorder.finish().unwrap();
    assert_eq!(summary.frames, BLOCK_FRAMES as u64);
    assert_eq!(summary.duration, Duration::from_nanos(21_768_707));
    assert_eq!(summary.clipped_samples, 0);

    let bytes = std::fs::read(file.path()).unwrap();
    let (header, samples) = parse_wav(&bytes);
    assert_eq!(header.channels, 2);
    assert_eq!(header.sample_rate, rate);
    assert_eq!(header.byte_rate, 176_400);
    assert_eq!(header.block_align, 4);
    assert_eq!(header.bits, 16);
    assert_eq!(header.data_len, (BLOCK_FRAMES * 4) as u32);
    assert_eq!(header.riff_len, header.data_len + 36);
    assert_eq!(bytes.len(), 44 + header.data_len as usize);
    assert_eq!(samples.len(), BLOCK_FRAMES * 2);
    for frame in samples.as_chunks::<2>().0 {
        assert_eq!(frame[0], 24_576, "microphone plus the left system tone");
        assert_eq!(frame[1], -8_192, "microphone plus the right system tone");
    }
}

#[test]
fn a_mono_wav_has_a_two_byte_block_align() {
    let file = TempFile::new("wav");
    let rate = 8_000;
    let spec = RecordingSpec::new(rate, Channels::Mono).unwrap();
    let mut recorder = Recorder::create(file.path(), spec).unwrap();
    recorder
        .add_source(
            Source::Microphone,
            &StreamSpec::new(Source::Microphone, rate, 1).unwrap(),
        )
        .unwrap();
    recorder
        .add_source(
            Source::SystemAudio,
            &StreamSpec::new(Source::SystemAudio, rate, 2).unwrap(),
        )
        .unwrap();

    let mut system_audio = vec![0.0_f32; BLOCK_FRAMES * 2];
    for frame in 0..BLOCK_FRAMES {
        system_audio[frame * 2] = 0.5;
        system_audio[frame * 2 + 1] = 0.25;
    }
    recorder
        .write(Source::Microphone, &[0.25; BLOCK_FRAMES])
        .unwrap();
    recorder.write(Source::SystemAudio, &system_audio).unwrap();
    assert_eq!(recorder.finish().unwrap().frames, BLOCK_FRAMES as u64);

    let bytes = std::fs::read(file.path()).unwrap();
    let (header, samples) = parse_wav(&bytes);
    assert_eq!(header.channels, 1);
    assert_eq!(header.sample_rate, rate);
    assert_eq!(header.byte_rate, 16_000);
    assert_eq!(header.block_align, 2);
    assert_eq!(header.data_len, (BLOCK_FRAMES * 2) as u32);
    assert_eq!(
        samples,
        vec![20_480; BLOCK_FRAMES],
        "the microphone plus the averaged system frame"
    );
}

#[test]
fn an_empty_wav_is_a_bare_header() {
    let file = TempFile::new("wav");
    let summary = Recorder::create(file.path(), recording_spec())
        .unwrap()
        .finish()
        .unwrap();
    assert_eq!(summary.frames, 0);
    assert_eq!(summary.clipped_samples, 0);
    assert_eq!(summary.duration, Duration::ZERO);

    let bytes = std::fs::read(file.path()).unwrap();
    assert_eq!(bytes.len(), 44, "an empty recording is a bare header");
    let (header, samples) = parse_wav(&bytes);
    assert_eq!(header.riff_len, 36);
    assert_eq!(header.data_len, 0);
    assert_eq!(samples, Vec::<i16>::new());
}

#[test]
fn audio_reaches_the_file_before_finish() {
    let file = TempFile::new("wav");
    let mut recorder = stereo_recorder(file.path());
    for _ in 0..10 {
        recorder
            .write(Source::Microphone, &[0.25; BLOCK_FRAMES])
            .unwrap();
        recorder
            .write(Source::SystemAudio, &[0.5; BLOCK_FRAMES * 2])
            .unwrap();
    }
    let length = std::fs::metadata(file.path()).unwrap().len();
    assert!(
        length > 44,
        "the samples reach the file while the recording runs"
    );

    recorder.finish().unwrap();
    let length = std::fs::metadata(file.path()).unwrap().len();
    assert_eq!(length, 44 + 10 * BLOCK_FRAMES as u64 * 4);
}

#[test]
fn a_wav_extension_is_matched_in_either_case() {
    for extension in ["WAV", "Wav"] {
        let file = TempFile::new(extension);
        let mut recorder = stereo_recorder(file.path());
        recorder
            .write(Source::Microphone, &[0.25; BLOCK_FRAMES])
            .unwrap();
        recorder
            .write(Source::SystemAudio, &[0.5; BLOCK_FRAMES * 2])
            .unwrap();
        assert_eq!(recorder.finish().unwrap().frames, BLOCK_FRAMES as u64);

        let bytes = std::fs::read(file.path()).unwrap();
        let (header, samples) = parse_wav(&bytes);
        assert_eq!(header.sample_rate, RATE, "`{extension}` must name WAV");
        assert_eq!(samples.len(), BLOCK_FRAMES * 2);
    }
}

#[test]
fn a_clipped_mix_reaches_full_scale_intact() {
    let file = TempFile::new("wav");
    let mut recorder = stereo_recorder(file.path());
    recorder
        .write(Source::Microphone, &[-0.9; BLOCK_FRAMES])
        .unwrap();
    recorder
        .write(Source::SystemAudio, &[-0.9; BLOCK_FRAMES * 2])
        .unwrap();
    recorder
        .write(Source::Microphone, &[0.9; BLOCK_FRAMES])
        .unwrap();
    recorder
        .write(Source::SystemAudio, &[0.9; BLOCK_FRAMES * 2])
        .unwrap();

    let summary = recorder.finish().unwrap();
    assert_eq!(summary.frames, 2 * BLOCK_FRAMES as u64);
    assert_eq!(
        summary.clipped_samples,
        4 * BLOCK_FRAMES as u64,
        "every channel of both blocks saturates"
    );

    let bytes = std::fs::read(file.path()).unwrap();
    let (_, samples) = parse_wav(&bytes);
    let block = BLOCK_FRAMES * 2;
    assert!(samples[..block].iter().all(|sample| *sample == i16::MIN));
    assert!(samples[block..].iter().all(|sample| *sample == i16::MAX));
}

#[test]
fn every_rate_a_spec_accepts_records_a_wav() {
    for rate in [8_000, 11_025, 22_050, 44_100, 96_000, 192_000, 384_000] {
        let file = TempFile::new("wav");
        let spec = RecordingSpec::new(rate, Channels::Mono).unwrap();
        let mut recorder = Recorder::create(file.path(), spec).unwrap();
        recorder
            .add_source(
                Source::Microphone,
                &StreamSpec::new(Source::Microphone, rate, 1).unwrap(),
            )
            .unwrap();
        recorder.write(Source::Microphone, &[0.5]).unwrap();
        assert_eq!(recorder.finish().unwrap().frames, 1);

        let bytes = std::fs::read(file.path()).unwrap();
        let (header, samples) = parse_wav(&bytes);
        assert_eq!(header.sample_rate, rate);
        assert_eq!(header.byte_rate, rate * 2, "{rate} Hz mono byte rate");
        assert_eq!(samples, vec![16_384]);
    }
}

#[test]
fn a_wav_recording_uses_the_tag_the_transcribe_readers_accept() {
    let file = TempFile::new("wav");
    let mut recorder = stereo_recorder(file.path());
    recorder
        .write(Source::Microphone, &[0.1; BLOCK_FRAMES])
        .unwrap();
    recorder
        .write(Source::SystemAudio, &[0.2; BLOCK_FRAMES * 2])
        .unwrap();
    recorder.finish().unwrap();

    // Walks the chunks exactly as `koe-transcribe`'s reader does.
    let bytes = std::fs::read(file.path()).unwrap();
    let mut format = 0_u16;
    let mut bits = 0_u16;
    let mut position = 12;
    let mut samples = Vec::new();
    while position + 8 <= bytes.len() {
        let id = &bytes[position..position + 4];
        let size =
            u32::from_le_bytes(bytes[position + 4..position + 8].try_into().unwrap()) as usize;
        let body = position + 8;
        let end = body
            .checked_add(size)
            .filter(|end| *end <= bytes.len())
            .unwrap();
        if id == b"fmt " {
            format = u16::from_le_bytes([bytes[body], bytes[body + 1]]);
            bits = u16::from_le_bytes([bytes[body + 14], bytes[body + 15]]);
        } else if id == b"data" {
            assert_eq!(format, 1, "the reader accepts only format tag 1");
            assert_eq!(bits, 16, "the reader accepts only 16-bit samples");
            samples = bytes[body..end]
                .as_chunks::<2>()
                .0
                .iter()
                .map(|pair| i16::from_le_bytes(*pair))
                .collect();
        }
        position = end + (size & 1);
    }
    assert_eq!(samples.len(), BLOCK_FRAMES * 2);
}
