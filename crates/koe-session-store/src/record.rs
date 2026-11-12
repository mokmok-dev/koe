//! The `session` record: one header, one body, one checksum.

use std::fs::{self, File};
use std::io::{self, Read, Write};
use std::path::{Component, Path};
use std::str;

use koe_capture_core::Source;
use koe_session::{CHECKPOINT_LEN, Checkpoint};
use koe_transcribe_core::{Segment, Transcript};

use crate::crc::crc32;
use crate::error::{DecodeError, Error, io_error};
use crate::text::{PARTIAL_FILE as TEXT_PARTIAL_FILE, TRANSCRIPT_FILE};
use crate::transcripts::Transcripts;

/// Name of the record inside a meeting directory.
pub const SESSION_FILE: &str = "session";
/// Record being written; never read.
pub const PARTIAL_FILE: &str = "session.partial";
/// Ids [`Store::create`][crate::Store::create] tries before giving up.
pub const CREATE_ATTEMPTS: u64 = 16;

const MAGIC: [u8; 4] = *b"KOEM";
const VERSION: u8 = 1;
pub const HEADER_LEN: usize = 24;
pub const TRAILER_LEN: usize = 4;
const FLAG_CHECKPOINT: u8 = 1;
const FLAG_RECORDING: u8 = 1 << 1;
// The flags are disjoint bits, so addition is their union.
const KNOWN_FLAGS: u8 = FLAG_CHECKPOINT + FLAG_RECORDING;
const MAX_NAME_BYTES: usize = 255;
pub const MAX_TEXT_BYTES: usize = 4 * 1024 * 1024;
pub const MAX_RECORD_LEN: usize =
    HEADER_LEN + MAX_NAME_BYTES + CHECKPOINT_LEN + 2 * MAX_TEXT_BYTES + TRAILER_LEN;

pub const SECTION_HEADER_LEN: usize = 12;
pub const SEGMENT_HEADER_LEN: usize = 20;

/// Everything a meeting directory persists besides the audio itself.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Record {
    pub checkpoint: Option<Checkpoint>,
    pub transcripts: Transcripts,
    pub recording: Option<Box<str>>,
}

/// Whether `name` can be the audio file of a meeting directory.
pub fn is_valid_name(name: &str) -> bool {
    is_plain_name(name) && !is_reserved_name(name)
}

/// Whether `name` is one plain file name: no directory part, no NUL, and not
/// a name a directory walk skips.
fn is_plain_name(name: &str) -> bool {
    if name.is_empty()
        || name.len() > MAX_NAME_BYTES
        || name.starts_with('.')
        || name.contains(['/', '\\', '\0'])
    {
        return false;
    }
    let mut components = Path::new(name).components();
    matches!(
        (components.next(), components.next()),
        (Some(Component::Normal(_)), None)
    )
}

/// Whether `name` belongs to a file the store writes itself.
///
/// Compared without case because the filesystems most meetings are stored on
/// fold case, where `Transcript.txt` names the rendered transcript too; a
/// trailing dot or space is ignored because Win32 strips both from a path
/// component. An 8.3 short name (`TRANSC~1.TXT`) is not detected: both
/// applications always name the audio `meeting.<ext>`, so only a caller of
/// `Meeting::set_recording` or a record written before the reservation can
/// carry one.
fn is_reserved_name(name: &str) -> bool {
    let name = name.trim_end_matches(['.', ' ']);
    [
        SESSION_FILE,
        PARTIAL_FILE,
        TRANSCRIPT_FILE,
        TEXT_PARTIAL_FILE,
    ]
    .iter()
    .any(|reserved| name.eq_ignore_ascii_case(reserved))
}

/// Encodes `record`, refusing a transcript that does not fit.
pub fn encode(record: &Record) -> Result<Vec<u8>, Error> {
    let mic = encode_section(
        Source::Microphone,
        record.transcripts.get(Source::Microphone),
    )?;
    let sys = encode_section(
        Source::SystemAudio,
        record.transcripts.get(Source::SystemAudio),
    )?;

    let name = record.recording.as_deref().unwrap_or_default();
    if record.recording.is_some() && !is_valid_name(name) {
        return Err(Error::InvalidRecording);
    }
    let name_len = u16::try_from(name.len()).map_err(|_| Error::InvalidRecording)?;
    let mic_len = text_len(Source::Microphone, &mic)?;
    let sys_len = text_len(Source::SystemAudio, &sys)?;

    // Both sections are capped and the name is at most `MAX_NAME_BYTES`, so the
    // sum fits; the checked form records that bound instead of assuming it.
    let total = HEADER_LEN
        .checked_add(name.len())
        .and_then(|len| len.checked_add(CHECKPOINT_LEN))
        .and_then(|len| len.checked_add(mic.len()))
        .and_then(|len| len.checked_add(sys.len()))
        .and_then(|len| len.checked_add(TRAILER_LEN))
        .ok_or(Error::RecordTooLarge { bytes: usize::MAX })?;
    let total_len = u32::try_from(total).map_err(|_| Error::RecordTooLarge { bytes: total })?;

    let has_checkpoint = record.checkpoint.is_some();
    let checkpoint = record
        .checkpoint
        .map_or([0u8; CHECKPOINT_LEN], Checkpoint::to_bytes);
    let mut flags = if has_checkpoint { FLAG_CHECKPOINT } else { 0 };
    if record.recording.is_some() {
        flags |= FLAG_RECORDING;
    }

    let mut bytes = Vec::with_capacity(total);
    bytes.extend_from_slice(&MAGIC);
    bytes.push(VERSION);
    bytes.push(flags);
    bytes.extend_from_slice(&[0, 0]);
    bytes.extend_from_slice(&mic_len.to_le_bytes());
    bytes.extend_from_slice(&sys_len.to_le_bytes());
    bytes.extend_from_slice(&total_len.to_le_bytes());
    bytes.extend_from_slice(&name_len.to_le_bytes());
    bytes.extend_from_slice(&[0, 0]);
    bytes.extend_from_slice(name.as_bytes());
    bytes.extend_from_slice(&checkpoint);
    bytes.extend_from_slice(&mic);
    bytes.extend_from_slice(&sys);
    let checksum = crc32(&bytes);
    bytes.extend_from_slice(&checksum.to_le_bytes());
    debug_assert_eq!(bytes.len(), total);
    Ok(bytes)
}

fn text_len(source: Source, section: &[u8]) -> Result<u32, Error> {
    u32::try_from(section.len()).map_err(|_| Error::TextTooLarge {
        source,
        bytes: section.len(),
    })
}

/// Encodes one text section, or nothing when the source holds no transcript.
fn encode_section(source: Source, transcript: Option<&Transcript>) -> Result<Vec<u8>, Error> {
    let Some(transcript) = transcript else {
        return Ok(Vec::new());
    };
    let segments = transcript.segments();
    let bytes = section_len(transcript).unwrap_or(usize::MAX);
    if bytes > MAX_TEXT_BYTES {
        return Err(Error::TextTooLarge { source, bytes });
    }
    let Ok(count) = u32::try_from(segments.len()) else {
        return Err(Error::TextTooLarge { source, bytes });
    };
    let too_large = || Error::TextTooLarge { source, bytes };

    let mut section = Vec::with_capacity(bytes);
    section.extend_from_slice(&transcript.dropped_ms().to_le_bytes());
    section.extend_from_slice(&count.to_le_bytes());
    for segment in segments {
        let text = segment.text().as_bytes();
        let Ok(text_len) = u32::try_from(text.len()) else {
            return Err(too_large());
        };
        section.extend_from_slice(&segment.start_ms().to_le_bytes());
        section.extend_from_slice(&segment.end_ms().to_le_bytes());
        section.extend_from_slice(&text_len.to_le_bytes());
        section.extend_from_slice(text);
    }
    Ok(section)
}

/// Encoded length of one text section, or [`None`] when it does not fit.
fn section_len(transcript: &Transcript) -> Option<usize> {
    let segments = transcript.segments();
    let mut len =
        SECTION_HEADER_LEN.checked_add(segments.len().checked_mul(SEGMENT_HEADER_LEN)?)?;
    for segment in segments {
        len = len.checked_add(segment.text().len())?;
    }
    Some(len)
}

/// Decodes a record written by [`encode`].
pub fn decode(bytes: &[u8]) -> Result<Record, DecodeError> {
    if bytes.len() > MAX_RECORD_LEN {
        return Err(DecodeError::TooLong { len: bytes.len() });
    }
    if bytes.len() < HEADER_LEN + CHECKPOINT_LEN + TRAILER_LEN {
        return Err(DecodeError::Length { len: bytes.len() });
    }
    let magic = [bytes[0], bytes[1], bytes[2], bytes[3]];
    if magic != MAGIC {
        return Err(DecodeError::Magic { found: magic });
    }
    if bytes[4] != VERSION {
        return Err(DecodeError::Version { found: bytes[4] });
    }
    let found = u32_at(bytes, bytes.len() - TRAILER_LEN);
    let expected = crc32(&bytes[..bytes.len() - TRAILER_LEN]);
    if found != expected {
        return Err(DecodeError::Checksum { expected, found });
    }

    let flags = bytes[5];
    if flags & !KNOWN_FLAGS != 0 {
        return Err(DecodeError::Flags { flags });
    }
    for offset in [6, 7, 22, 23] {
        if bytes[offset] != 0 {
            return Err(DecodeError::Reserved { offset });
        }
    }
    let declared = u32_at(bytes, 16);
    if usize::try_from(declared).unwrap_or(usize::MAX) != bytes.len() {
        return Err(DecodeError::TotalLength {
            declared,
            actual: bytes.len(),
        });
    }

    let name_len = u16_at(bytes, 20);
    let has_recording = flags & FLAG_RECORDING != 0;
    if has_recording && name_len == 0 {
        return Err(DecodeError::RecordingName { len: 0 });
    }
    if !has_recording && name_len != 0 {
        return Err(DecodeError::UnexpectedRecording);
    }

    decode_body(bytes, flags, declared, name_len)
}

/// Decodes the body once the header has named a consistent total length.
fn decode_body(
    bytes: &[u8],
    flags: u8,
    declared: u32,
    name_len: u16,
) -> Result<Record, DecodeError> {
    let has_recording = flags & FLAG_RECORDING != 0;
    let mic_stored = u32_at(bytes, 8);
    let sys_stored = u32_at(bytes, 12);
    let mic_len = usize::try_from(mic_stored).unwrap_or(usize::MAX);
    let sys_len = usize::try_from(sys_stored).unwrap_or(usize::MAX);

    let end = HEADER_LEN
        .checked_add(usize::from(name_len))
        .and_then(|end| end.checked_add(CHECKPOINT_LEN))
        .and_then(|end| end.checked_add(mic_len))
        .and_then(|end| end.checked_add(sys_len))
        .and_then(|end| end.checked_add(TRAILER_LEN));
    if end != Some(bytes.len()) {
        return Err(DecodeError::TotalLength {
            declared,
            actual: end.unwrap_or(usize::MAX),
        });
    }
    let name_end = HEADER_LEN + usize::from(name_len);
    let checkpoint_end = name_end + CHECKPOINT_LEN;
    let mic_end = checkpoint_end + mic_len;
    let sys_end = mic_end + sys_len;

    let recording = if has_recording {
        let name = str::from_utf8(&bytes[HEADER_LEN..name_end])
            .map_err(|_| DecodeError::RecordingName { len: name_len })?;
        if !is_plain_name(name) {
            return Err(DecodeError::RecordingName { len: name_len });
        }
        if is_reserved_name(name) {
            return Err(DecodeError::ReservedName { len: name_len });
        }
        Some(name.into())
    } else {
        None
    };

    let checkpoint_bytes = &bytes[name_end..checkpoint_end];
    let checkpoint = if flags & FLAG_CHECKPOINT != 0 {
        Some(Checkpoint::from_bytes(checkpoint_bytes).map_err(DecodeError::Checkpoint)?)
    } else {
        if checkpoint_bytes.iter().any(|&byte| byte != 0) {
            return Err(DecodeError::UnexpectedCheckpoint);
        }
        None
    };

    let mut transcripts = Transcripts::new();
    transcripts.set(
        Source::Microphone,
        decode_section(
            Source::Microphone,
            &bytes[checkpoint_end..mic_end],
            mic_stored,
        )?,
    );
    transcripts.set(
        Source::SystemAudio,
        decode_section(Source::SystemAudio, &bytes[mic_end..sys_end], sys_stored)?,
    );

    Ok(Record {
        checkpoint,
        transcripts,
        recording,
    })
}

/// Decodes one text section, or nothing when its length is zero.
fn decode_section(
    source: Source,
    bytes: &[u8],
    len: u32,
) -> Result<Option<Transcript>, DecodeError> {
    if bytes.is_empty() {
        return Ok(None);
    }
    if !(SECTION_HEADER_LEN..=MAX_TEXT_BYTES).contains(&bytes.len()) {
        return Err(DecodeError::SectionLength { source, len });
    }
    let dropped_ms = u64_at(bytes, 0);
    let count = u32_at(bytes, 8);
    let capacity = (bytes.len() - SECTION_HEADER_LEN) / SEGMENT_HEADER_LEN;
    if usize::try_from(count).unwrap_or(usize::MAX) > capacity {
        return Err(DecodeError::SegmentCount { source, count, len });
    }

    let mut spans = Vec::with_capacity(usize::try_from(count).unwrap_or(capacity));
    let mut offset = SECTION_HEADER_LEN;
    for index in 0..count {
        if bytes.len() - offset < SEGMENT_HEADER_LEN {
            return Err(DecodeError::Segment { source, index });
        }
        let start_ms = u64_at(bytes, offset);
        let end_ms = u64_at(bytes, offset + 8);
        let text_len = u32_at(bytes, offset + 16);
        let text_start = offset + SEGMENT_HEADER_LEN;
        let text_end = text_start
            .checked_add(usize::try_from(text_len).unwrap_or(usize::MAX))
            .ok_or(DecodeError::Segment { source, index })?;
        if text_end > bytes.len() {
            return Err(DecodeError::Segment { source, index });
        }
        let text = str::from_utf8(&bytes[text_start..text_end])
            .map_err(|_| DecodeError::Text { source, index })?;
        spans.push((start_ms, end_ms, text));
        offset = text_end;
    }
    if offset != bytes.len() {
        return Err(DecodeError::Section { source });
    }

    let segments = spans
        .into_iter()
        .map(|(start_ms, end_ms, text)| Segment::new(start_ms, end_ms, text))
        .collect();
    Ok(Some(Transcript::new(segments, dropped_ms)))
}

const fn u16_at(bytes: &[u8], offset: usize) -> u16 {
    u16::from_le_bytes([bytes[offset], bytes[offset + 1]])
}

const fn u32_at(bytes: &[u8], offset: usize) -> u32 {
    u32::from_le_bytes([
        bytes[offset],
        bytes[offset + 1],
        bytes[offset + 2],
        bytes[offset + 3],
    ])
}

const fn u64_at(bytes: &[u8], offset: usize) -> u64 {
    u64::from_le_bytes([
        bytes[offset],
        bytes[offset + 1],
        bytes[offset + 2],
        bytes[offset + 3],
        bytes[offset + 4],
        bytes[offset + 5],
        bytes[offset + 6],
        bytes[offset + 7],
    ])
}

/// Writes `record` as `directory`'s `session`, replacing whatever was there.
pub fn write(directory: &Path, record: &Record) -> Result<(), Error> {
    let bytes = encode(record)?;
    let partial = directory.join(PARTIAL_FILE);
    let mut file = File::create(&partial).map_err(io_error("create"))?;
    file.write_all(&bytes).map_err(io_error("write"))?;
    file.sync_all().map_err(io_error("sync"))?;
    drop(file);
    fs::rename(&partial, directory.join(SESSION_FILE)).map_err(io_error("rename"))?;
    sync_directory(directory);
    Ok(())
}

/// Reads `directory`'s `session`, or [`None`] when there is no record.
///
/// A file larger than any record stops at the bound instead of being read
/// whole, so an attacked or corrupted file costs one buffer of that size.
pub fn read(directory: &Path) -> Result<Option<Record>, Error> {
    let file = match File::open(directory.join(SESSION_FILE)) {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(Error::Io {
                operation: "open",
                kind: error.kind(),
            });
        }
    };
    let mut bytes = Vec::new();
    file.take((MAX_RECORD_LEN + 1) as u64)
        .read_to_end(&mut bytes)
        .map_err(io_error("read"))?;
    decode(&bytes).map(Some).map_err(Error::Decode)
}

/// Makes a rename inside `directory` durable.
///
/// A failure is ignored: the record is already renamed and readable, and some
/// filesystems reject a directory sync.
#[cfg(unix)]
pub fn sync_directory(directory: &Path) {
    if let Ok(file) = File::open(directory) {
        let _ = file.sync_all();
    }
}

#[cfg(not(unix))]
pub const fn sync_directory(_directory: &Path) {}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use proptest::prelude::*;

    use koe_session::{
        Fault, Input, Outcome, Permission, Permissions, SegmentOutcome, SegmentResult, Session,
        SourceSet, Stage,
    };

    use super::*;

    const NAME: &str = "meeting.ogg";
    const MIC_LEN: usize = 62;
    const TOTAL_LEN: usize = 115;

    const DRAFT_IMAGE: [u8; HEADER_LEN + CHECKPOINT_LEN + TRAILER_LEN] = [
        75, 79, 69, 77, 1, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 42, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
        0, 0, 0, 0, 0, 0, 0, 0, 0, 166, 193, 44, 174,
    ];

    const SETTLED_IMAGE: [u8; TOTAL_LEN] = [
        75, 79, 69, 77, 1, 3, 0, 0, 62, 0, 0, 0, 0, 0, 0, 0, 115, 0, 0, 0, 11, 0, 0, 0, 109, 101,
        101, 116, 105, 110, 103, 46, 111, 103, 103, 75, 79, 69, 83, 1, 1, 3, 3, 1, 1, 33, 141, 104,
        127, 120, 0, 0, 0, 0, 0, 0, 0, 2, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 244, 1, 0, 0, 0, 0, 0,
        0, 5, 0, 0, 0, 104, 101, 108, 108, 111, 244, 1, 0, 0, 0, 0, 0, 0, 132, 3, 0, 0, 0, 0, 0, 0,
        5, 0, 0, 0, 119, 111, 114, 108, 100, 39, 45, 10, 155,
    ];

    fn u32_len(len: usize) -> u32 {
        u32::try_from(len).unwrap()
    }

    fn u16_len(len: usize) -> u16 {
        u16::try_from(len).unwrap()
    }

    fn granted() -> Permissions {
        Permissions::new(Permission::Granted, Permission::Granted)
    }

    fn drive(session: Session, inputs: &[Input]) -> Session {
        inputs.iter().fold(session, |session, input| {
            session.step(*input).unwrap().session
        })
    }

    const fn segment_closed(source: Source, committed: bool) -> Input {
        Input::SegmentClosed {
            source,
            result: SegmentResult {
                outcome: SegmentOutcome::Complete,
                committed,
            },
        }
    }

    fn recording(sources: SourceSet) -> Session {
        let mut inputs = vec![Input::Start(sources)];
        for source in sources.iter() {
            inputs.push(Input::DeviceOpened(source));
        }
        let session = drive(Session::new(granted()), &inputs);
        assert_eq!(session.stage(), Stage::Recording);
        session
    }

    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    enum FaultKind {
        Capture,
        Device,
        Engine,
        Permission,
    }

    const fn fault_input(kind: FaultKind, source: Source) -> Input {
        match kind {
            FaultKind::Capture => Input::CaptureFailed(source),
            FaultKind::Device => Input::DeviceFailed(source),
            FaultKind::Engine => Input::EngineFailed(source),
            FaultKind::Permission => Input::PermissionRevoked(source),
        }
    }

    const fn fault_of(kind: FaultKind, source: Source) -> Fault {
        match kind {
            FaultKind::Capture => Fault::Capture(source),
            FaultKind::Device => Fault::Device(source),
            FaultKind::Engine => Fault::Engine(source),
            FaultKind::Permission => Fault::Permission(source),
        }
    }

    /// Drives a session to a settled stage, or [`None`] when the state machine
    /// cannot get there from this combination of sources and fault.
    fn settled(
        stage: Stage,
        sources: SourceSet,
        fault: Option<(FaultKind, Source)>,
    ) -> Option<Session> {
        let reviewing = matches!(stage, Stage::Reviewing | Stage::Confirmed);
        let mut session = recording(sources);
        if let Some((kind, source)) = fault {
            session = session.step(fault_input(kind, source)).ok()?.session;
            if session.outcome() != Some(Outcome::Incomplete(fault_of(kind, source))) {
                return None;
            }
        }
        if session.stage() == Stage::Recording {
            let input = if reviewing { Input::Stop } else { Input::Pause };
            session = session.step(input).ok()?.session;
        } else if reviewing && session.stage() == Stage::Paused {
            session = session.step(Input::Stop).ok()?.session;
        }
        loop {
            let finishing = session.finishing_sources();
            if finishing.is_empty() {
                break;
            }
            let mut next = session;
            for source in finishing.iter() {
                next = next.step(segment_closed(source, true)).ok()?.session;
            }
            session = next;
        }
        if stage == Stage::Confirmed {
            session = session.step(Input::Confirm).ok()?.session;
        }
        (session.stage() == stage).then_some(session)
    }

    fn checkpoints() -> Vec<Checkpoint> {
        let mut checkpoints = Vec::new();
        for stage in [Stage::Paused, Stage::Reviewing, Stage::Confirmed] {
            for sources in [
                SourceSet::MICROPHONE,
                SourceSet::SYSTEM_AUDIO,
                SourceSet::MEETING,
            ] {
                let faults = std::iter::once(None).chain(sources.iter().flat_map(|source| {
                    [
                        FaultKind::Capture,
                        FaultKind::Device,
                        FaultKind::Engine,
                        FaultKind::Permission,
                    ]
                    .map(|kind| Some((kind, source)))
                }));
                for fault in faults {
                    if let Some(session) = settled(stage, sources, fault) {
                        checkpoints.push(Checkpoint::of(&session).unwrap());
                    }
                }
            }
        }
        checkpoints
    }

    fn settled_session() -> Session {
        settled(
            Stage::Paused,
            SourceSet::MEETING,
            Some((FaultKind::Capture, Source::SystemAudio)),
        )
        .unwrap()
    }

    fn draft_record() -> Record {
        Record::default()
    }

    fn settled_record() -> Record {
        let mut transcripts = Transcripts::new();
        transcripts.set(
            Source::Microphone,
            Some(Transcript::new(
                vec![
                    Segment::new(0, 500, "hello"),
                    Segment::new(500, 900, "world"),
                ],
                120,
            )),
        );
        Record {
            checkpoint: Checkpoint::of(&settled_session()),
            transcripts,
            recording: Some(NAME.into()),
        }
    }

    /// Recomputes the trailing checksum so a test can corrupt another byte.
    fn seal(bytes: &mut [u8]) {
        let len = bytes.len();
        let checksum = crc32(&bytes[..len - TRAILER_LEN]);
        bytes[len - TRAILER_LEN..].copy_from_slice(&checksum.to_le_bytes());
    }

    fn transcript_choices() -> Vec<Option<Transcript>> {
        vec![
            None,
            Some(Transcript::new(Vec::new(), 0)),
            Some(Transcript::new(
                vec![
                    Segment::new(0, 500, "hello"),
                    Segment::new(500, 900, "world"),
                ],
                120,
            )),
            Some(Transcript::new(
                vec![Segment::new(0, 500, "hello"), Segment::new(500, 500, "")],
                0,
            )),
        ]
    }

    fn settled_bytes() -> Vec<u8> {
        SETTLED_IMAGE.to_vec()
    }

    fn name_end() -> usize {
        HEADER_LEN + NAME.len()
    }

    fn mic_start() -> usize {
        name_end() + CHECKPOINT_LEN
    }

    fn mic_text_start() -> usize {
        mic_start() + SECTION_HEADER_LEN + SEGMENT_HEADER_LEN
    }

    #[test]
    fn the_crc_check_value_is_the_standard_one() {
        assert_eq!(crc32(b"123456789"), 0xCBF4_3926);
    }

    #[test]
    fn the_draft_image_is_stable() {
        let bytes = encode(&draft_record()).unwrap();
        assert_eq!(bytes, DRAFT_IMAGE);
        assert_eq!(u32_at(&bytes, 16), 42);
        assert_eq!(u32_at(&bytes, bytes.len() - TRAILER_LEN), 0xAE2C_C1A6);
        assert_eq!(decode(&bytes), Ok(draft_record()));
    }

    #[test]
    fn the_settled_image_is_stable() {
        let record = settled_record();
        let bytes = encode(&record).unwrap();
        assert_eq!(bytes, SETTLED_IMAGE);
        assert_eq!(u32_at(&bytes, 8), u32_len(MIC_LEN));
        assert_eq!(u32_at(&bytes, 12), 0);
        assert_eq!(u32_at(&bytes, 16), u32_len(TOTAL_LEN));
        assert_eq!(u32_at(&bytes, 20), u32_len(NAME.len()));
        assert_eq!(u32_at(&bytes, bytes.len() - TRAILER_LEN), 0x9B0A_2D27);
        assert_eq!(decode(&bytes), Ok(record));
    }

    #[test]
    fn every_valid_record_round_trips() {
        let checkpoints = checkpoints();
        assert_eq!(checkpoints.len(), 49);
        let names: [Option<Box<str>>; 2] = [None, Some(NAME.into())];
        for checkpoint in checkpoints {
            for mic in transcript_choices() {
                for sys in transcript_choices() {
                    for recording in &names {
                        let mut transcripts = Transcripts::new();
                        transcripts.set(Source::Microphone, mic.clone());
                        transcripts.set(Source::SystemAudio, sys.clone());
                        let record = Record {
                            checkpoint: Some(checkpoint),
                            transcripts,
                            recording: recording.clone(),
                        };
                        let bytes = encode(&record).unwrap();
                        assert_eq!(decode(&bytes), Ok(record.clone()));
                        let draft = Record {
                            checkpoint: None,
                            ..record
                        };
                        let bytes = encode(&draft).unwrap();
                        assert_eq!(decode(&bytes), Ok(draft));
                    }
                }
            }
        }
    }

    #[test]
    fn every_single_byte_corruption_is_rejected() {
        for offset in 0..SETTLED_IMAGE.len() {
            for value in 0u8..=u8::MAX {
                if value == SETTLED_IMAGE[offset] {
                    continue;
                }
                let mut corrupt = SETTLED_IMAGE;
                corrupt[offset] = value;
                assert!(decode(&corrupt).is_err(), "offset {offset} value {value}");
            }
        }
    }

    #[test]
    fn every_truncated_length_is_rejected() {
        let bytes = settled_bytes();
        for len in 0..bytes.len() {
            assert!(decode(&bytes[..len]).is_err(), "length {len}");
        }
    }

    #[test]
    fn a_length_below_the_minimum_names_the_minimum() {
        let bytes = settled_bytes();
        let minimum = HEADER_LEN + CHECKPOINT_LEN + TRAILER_LEN;
        assert_eq!(
            decode(&bytes[..minimum - 1]),
            Err(DecodeError::Length { len: minimum - 1 })
        );
    }

    #[test]
    fn a_record_at_the_cap_is_accepted() {
        let text = "x".repeat(MAX_TEXT_BYTES - SECTION_HEADER_LEN - SEGMENT_HEADER_LEN);
        let transcript = Transcript::new(vec![Segment::new(0, 1, text)], 0);
        let mut transcripts = Transcripts::new();
        transcripts.set(Source::Microphone, Some(transcript.clone()));
        transcripts.set(Source::SystemAudio, Some(transcript));
        let name = format!("{}.ogg", "x".repeat(MAX_NAME_BYTES - 4));
        let record = Record {
            checkpoint: None,
            transcripts,
            recording: Some(name.into()),
        };
        let bytes = encode(&record).unwrap();
        assert_eq!(bytes.len(), MAX_RECORD_LEN);
        assert_eq!(decode(&bytes), Ok(record));
    }

    #[test]
    fn the_little_endian_readers_use_their_offset() {
        let bytes = [0x11, 0x22, 0x33, 0x44, 0x55, 0x66, 0x77, 0x88, 0x99];
        assert_eq!(u16_at(&bytes, 1), 0x3322);
        assert_eq!(u32_at(&bytes, 2), 0x6655_4433);
        assert_eq!(u64_at(&bytes, 1), 0x9988_7766_5544_3322);
    }

    #[test]
    fn one_trailing_byte_is_rejected() {
        let mut bytes = settled_bytes();
        bytes.push(0);
        assert!(decode(&bytes).is_err());
    }

    #[test]
    fn an_oversized_image_is_too_long() {
        let bytes = vec![0u8; MAX_RECORD_LEN + 1];
        assert_eq!(
            decode(&bytes),
            Err(DecodeError::TooLong {
                len: MAX_RECORD_LEN + 1
            })
        );
    }

    #[test]
    fn a_bad_magic_is_reported() {
        let mut bytes = settled_bytes();
        bytes[0] = b'X';
        seal(&mut bytes);
        assert_eq!(decode(&bytes), Err(DecodeError::Magic { found: *b"XOEM" }));
    }

    #[test]
    fn an_unknown_version_is_reported() {
        let mut bytes = settled_bytes();
        bytes[4] = 2;
        seal(&mut bytes);
        assert_eq!(decode(&bytes), Err(DecodeError::Version { found: 2 }));
    }

    #[test]
    fn a_bad_checksum_is_reported() {
        let mut bytes = settled_bytes();
        let len = bytes.len();
        bytes[mic_text_start()] ^= 0xFF;
        let expected = crc32(&bytes[..len - TRAILER_LEN]);
        let found = u32_at(&bytes, len - TRAILER_LEN);
        assert_eq!(
            decode(&bytes),
            Err(DecodeError::Checksum { expected, found })
        );
    }

    #[test]
    fn an_unknown_flag_is_reported() {
        let mut bytes = settled_bytes();
        bytes[5] |= 0b1000_0000;
        seal(&mut bytes);
        assert_eq!(
            decode(&bytes),
            Err(DecodeError::Flags { flags: 0b1000_0011 })
        );
    }

    #[test]
    fn a_non_zero_reserved_byte_is_reported() {
        for offset in [6, 7, 22, 23] {
            let mut bytes = settled_bytes();
            bytes[offset] = 1;
            seal(&mut bytes);
            assert_eq!(decode(&bytes), Err(DecodeError::Reserved { offset }));
        }
    }

    #[test]
    fn a_bad_total_length_is_reported() {
        let mut bytes = settled_bytes();
        bytes[16..20].copy_from_slice(&(u32_len(TOTAL_LEN) + 1).to_le_bytes());
        seal(&mut bytes);
        assert_eq!(
            decode(&bytes),
            Err(DecodeError::TotalLength {
                declared: u32_len(TOTAL_LEN) + 1,
                actual: TOTAL_LEN,
            })
        );
    }

    #[test]
    fn a_clear_checkpoint_flag_with_checkpoint_bytes_is_reported() {
        let mut bytes = settled_bytes();
        bytes[5] &= !FLAG_CHECKPOINT;
        seal(&mut bytes);
        assert_eq!(decode(&bytes), Err(DecodeError::UnexpectedCheckpoint));
    }

    #[test]
    fn a_clear_recording_flag_with_a_name_length_is_reported() {
        let mut bytes = settled_bytes();
        bytes[5] &= !FLAG_RECORDING;
        seal(&mut bytes);
        assert_eq!(decode(&bytes), Err(DecodeError::UnexpectedRecording));
    }

    #[test]
    fn a_set_recording_flag_without_a_name_is_reported() {
        let mut bytes = settled_bytes();
        let name_end = HEADER_LEN + NAME.len();
        bytes.drain(HEADER_LEN..name_end);
        bytes[20..22].copy_from_slice(&0u16.to_le_bytes());
        bytes[16..20].copy_from_slice(&(u32_len(TOTAL_LEN) - u32_len(NAME.len())).to_le_bytes());
        seal(&mut bytes);
        assert_eq!(decode(&bytes), Err(DecodeError::RecordingName { len: 0 }));
    }

    #[test]
    fn a_recorded_name_that_is_not_a_file_name_is_reported() {
        let mut bytes = settled_bytes();
        bytes[HEADER_LEN] = b'.';
        seal(&mut bytes);
        assert_eq!(
            decode(&bytes),
            Err(DecodeError::RecordingName {
                len: u16_len(NAME.len())
            })
        );
    }

    #[test]
    fn a_recorded_name_the_store_reserves_is_reported_as_reserved() {
        for reserved in ["Transcript.TXT", "transcript.txt.", "transcript.txt "] {
            let placeholder = "x".repeat(reserved.len() - 4) + ".ogg";
            assert_eq!(placeholder.len(), reserved.len());
            let record = Record {
                recording: Some(placeholder.into()),
                ..Record::default()
            };
            let mut bytes = encode(&record).unwrap();
            bytes[HEADER_LEN..HEADER_LEN + reserved.len()].copy_from_slice(reserved.as_bytes());
            seal(&mut bytes);
            assert_eq!(
                decode(&bytes),
                Err(DecodeError::ReservedName {
                    len: u16_len(reserved.len())
                })
            );
        }
    }

    #[test]
    fn a_reserved_name_is_not_a_valid_recording_name() {
        for name in [
            "session",
            "SESSION",
            "session ",
            "session.",
            "session.partial",
            "transcript.txt",
            "Transcript.txt",
            "transcript.txt.",
            "transcript.txt ",
            "transcript.partial",
        ] {
            assert!(!is_valid_name(name), "{name:?} must be reserved");
            let record = Record {
                recording: Some(name.into()),
                ..Record::default()
            };
            assert_eq!(encode(&record), Err(Error::InvalidRecording), "{name:?}");
        }
        assert!(is_valid_name("meeting.ogg"));
        assert!(is_valid_name("Session notes.ogg"));
    }

    #[test]
    fn non_utf8_text_is_reported() {
        let mut bytes = settled_bytes();
        bytes[mic_text_start()] = 0xFF;
        seal(&mut bytes);
        assert_eq!(
            decode(&bytes),
            Err(DecodeError::Text {
                source: Source::Microphone,
                index: 0,
            })
        );
    }

    #[test]
    fn a_segment_count_larger_than_the_section_is_reported() {
        let mut bytes = settled_bytes();
        bytes[mic_start() + 8..mic_start() + 12].copy_from_slice(&3u32.to_le_bytes());
        seal(&mut bytes);
        assert_eq!(
            decode(&bytes),
            Err(DecodeError::SegmentCount {
                source: Source::Microphone,
                count: 3,
                len: u32_len(MIC_LEN),
            })
        );
    }

    #[test]
    fn a_segment_running_past_the_section_is_reported() {
        let mut bytes = settled_bytes();
        let last = mic_start() + MIC_LEN - "world".len() - 4;
        bytes[last..last + 4].copy_from_slice(&6u32.to_le_bytes());
        seal(&mut bytes);
        assert_eq!(
            decode(&bytes),
            Err(DecodeError::Segment {
                source: Source::Microphone,
                index: 1,
            })
        );
    }

    #[test]
    fn a_segment_header_past_the_section_is_reported() {
        let mut bytes = settled_bytes();
        let text_len_at = mic_start() + SECTION_HEADER_LEN + 16;
        let text_start = mic_start() + SECTION_HEADER_LEN + SEGMENT_HEADER_LEN;
        bytes[text_len_at..text_len_at + 4].copy_from_slice(&30u32.to_le_bytes());
        bytes[text_start..mic_start() + MIC_LEN].fill(b'a');
        seal(&mut bytes);
        assert_eq!(
            decode(&bytes),
            Err(DecodeError::Segment {
                source: Source::Microphone,
                index: 1,
            })
        );
    }

    #[test]
    fn a_section_with_trailing_bytes_is_reported() {
        let mut bytes = settled_bytes();
        let mic_end = mic_start() + MIC_LEN;
        bytes.insert(mic_end, 0);
        bytes[8..12].copy_from_slice(&(u32_len(MIC_LEN) + 1).to_le_bytes());
        bytes[16..20].copy_from_slice(&(u32_len(TOTAL_LEN) + 1).to_le_bytes());
        seal(&mut bytes);
        assert_eq!(
            decode(&bytes),
            Err(DecodeError::Section {
                source: Source::Microphone,
            })
        );
    }

    #[test]
    fn a_section_over_the_cap_is_reported() {
        let over = MAX_TEXT_BYTES + 1;
        let total = HEADER_LEN + NAME.len() + CHECKPOINT_LEN + over + TRAILER_LEN;
        let mut bytes = vec![0u8; total];
        bytes[..4].copy_from_slice(&MAGIC);
        bytes[4] = VERSION;
        bytes[5] = FLAG_CHECKPOINT | FLAG_RECORDING;
        bytes[8..12].copy_from_slice(&u32::try_from(over).unwrap().to_le_bytes());
        bytes[16..20].copy_from_slice(&u32::try_from(total).unwrap().to_le_bytes());
        bytes[20..22].copy_from_slice(&(u16_len(NAME.len())).to_le_bytes());
        bytes[HEADER_LEN..name_end()].copy_from_slice(NAME.as_bytes());
        bytes[name_end()..mic_start()].copy_from_slice(&SETTLED_IMAGE[35..49]);
        seal(&mut bytes);
        assert_eq!(
            decode(&bytes),
            Err(DecodeError::SectionLength {
                source: Source::Microphone,
                len: u32::try_from(over).unwrap(),
            })
        );
    }

    #[test]
    fn encode_refuses_a_transcript_over_the_cap() {
        let text = "x".repeat(MAX_TEXT_BYTES);
        let mut transcripts = Transcripts::new();
        transcripts.set(
            Source::Microphone,
            Some(Transcript::new(vec![Segment::new(0, 1, text)], 0)),
        );
        let record = Record {
            checkpoint: None,
            transcripts,
            recording: None,
        };
        assert_eq!(
            encode(&record),
            Err(Error::TextTooLarge {
                source: Source::Microphone,
                bytes: SECTION_HEADER_LEN + SEGMENT_HEADER_LEN + MAX_TEXT_BYTES,
            })
        );
    }

    proptest! {
        #[test]
        fn decode_never_panics(bytes in prop::collection::vec(any::<u8>(), 0..=512)) {
            let _ = decode(&bytes);
        }
    }
}
