//! Durable checkpoint of a session's restart-safe state.
//!
//! A checkpoint keeps the settled stage, the meeting's sources, and the
//! committed transcript progress. It drops everything that belongs to the
//! running process: open and finishing streams, pending device opens, volatile
//! text, and permissions. [`Checkpoint::of`] is total over the settled stages
//! and [`None`] until a meeting has captured audio.

use core::fmt;

use koe_capture_core::Source;

use crate::permission::Permissions;
use crate::sources::SourceSet;
use crate::state::{Confirmed, Next, Origin, Paused, Phase, Progress, Reviewing, Session};
use crate::transcript::{Fault, Outcome, Transcript};

/// Number of bytes [`Checkpoint::to_bytes`] writes and
/// [`Checkpoint::from_bytes`] accepts.
pub const CHECKPOINT_LEN: usize = 14;

const MAGIC: [u8; 4] = *b"KOES";
const VERSION: u8 = 1;
const NO_FAULT: u8 = 0xFF;

const STAGE_PAUSED: u8 = 1;
const STAGE_REVIEWING: u8 = 2;
const STAGE_CONFIRMED: u8 = 3;

const OUTCOME_COMPLETE: u8 = 0;
const OUTCOME_CAPTURE: u8 = 1;
const OUTCOME_DEVICE: u8 = 2;
const OUTCOME_ENGINE: u8 = 3;
const OUTCOME_PERMISSION: u8 = 4;

/// The stages a checkpoint can name: the ones with no capture in flight.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Settled {
    Paused,
    Reviewing,
    Confirmed,
}

impl Settled {
    const fn tag(self) -> u8 {
        match self {
            Self::Paused => STAGE_PAUSED,
            Self::Reviewing => STAGE_REVIEWING,
            Self::Confirmed => STAGE_CONFIRMED,
        }
    }

    const fn from_tag(tag: u8) -> Option<Self> {
        match tag {
            STAGE_PAUSED => Some(Self::Paused),
            STAGE_REVIEWING => Some(Self::Reviewing),
            STAGE_CONFIRMED => Some(Self::Confirmed),
            _ => None,
        }
    }
}

/// The restart-safe state of one meeting.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Checkpoint {
    stage: Settled,
    sources: SourceSet,
    progress: Progress,
}

impl Checkpoint {
    /// The checkpoint `session` settles into, or [`None`] while the meeting has
    /// captured no audio.
    #[must_use]
    pub const fn of(session: &Session) -> Option<Self> {
        let (stage, sources, progress) = match session.phase {
            Phase::Idle => return None,
            Phase::Preparing(preparing) => match preparing.origin {
                Origin::Fresh => return None,
                Origin::Resume(progress) => (Settled::Paused, preparing.sources, progress),
            },
            Phase::Recording(recording) => (Settled::Paused, recording.sources, recording.progress),
            Phase::Paused(paused) => (Settled::Paused, paused.sources, paused.progress),
            Phase::Closing(closing) => {
                let stage = match closing.next {
                    Next::Pause => Settled::Paused,
                    Next::Stop => Settled::Reviewing,
                };
                (stage, closing.sources, closing.progress)
            }
            Phase::Reviewing(reviewing) => {
                (Settled::Reviewing, reviewing.sources, reviewing.progress)
            }
            Phase::Confirmed(confirmed) => {
                (Settled::Confirmed, confirmed.sources, confirmed.progress)
            }
        };
        Some(Self {
            stage,
            sources,
            progress: settled(progress),
        })
    }

    /// Encodes the checkpoint as [`CHECKPOINT_LEN`] bytes.
    #[must_use]
    pub fn to_bytes(self) -> [u8; CHECKPOINT_LEN] {
        let mut bytes = [0u8; CHECKPOINT_LEN];
        bytes[..MAGIC.len()].copy_from_slice(&MAGIC);
        bytes[4] = VERSION;
        bytes[5] = self.stage.tag();
        bytes[6] = self.sources.bits();
        bytes[7] = committed_bits(self.progress.transcript);
        bytes[8] = outcome_tag(self.progress.outcome);
        bytes[9] = match self.progress.outcome {
            Outcome::Complete => NO_FAULT,
            Outcome::Incomplete(fault) => source_tag(fault_source(fault)),
        };
        let checksum = crc32(&bytes[..10]).to_le_bytes();
        bytes[10..].copy_from_slice(&checksum);
        bytes
    }

    /// Decodes a checkpoint written by [`Self::to_bytes`].
    ///
    /// # Errors
    ///
    /// Returns [`DecodeError`] when the length, magic, version, checksum, or
    /// one of the fields is not a checkpoint this crate wrote.
    pub fn from_bytes(bytes: &[u8]) -> Result<Self, DecodeError> {
        if bytes.len() != CHECKPOINT_LEN {
            return Err(DecodeError::Length { len: bytes.len() });
        }
        let magic = [bytes[0], bytes[1], bytes[2], bytes[3]];
        if magic != MAGIC {
            return Err(DecodeError::Magic { found: magic });
        }
        if bytes[4] != VERSION {
            return Err(DecodeError::Version { found: bytes[4] });
        }
        let found = u32::from_le_bytes([bytes[10], bytes[11], bytes[12], bytes[13]]);
        let expected = crc32(&bytes[..10]);
        if found != expected {
            return Err(DecodeError::Checksum { expected, found });
        }
        let stage = Settled::from_tag(bytes[5]).ok_or(DecodeError::StageTag { tag: bytes[5] })?;
        let sources = match SourceSet::from_bits(bytes[6]) {
            Some(sources) if !sources.is_empty() => sources,
            _ => return Err(DecodeError::Sources { mask: bytes[6] }),
        };
        let mut transcript = Transcript::new();
        let committed =
            SourceSet::from_bits(bytes[7]).ok_or(DecodeError::CommittedMask { mask: bytes[7] })?;
        for source in committed.iter() {
            if !sources.contains(source) {
                return Err(DecodeError::CommittedOutsideSources { source });
            }
            transcript.set_committed(source, true);
        }
        let kind = bytes[8];
        if kind > OUTCOME_PERMISSION {
            return Err(DecodeError::OutcomeKind { tag: kind });
        }
        let fault = if bytes[9] == NO_FAULT {
            None
        } else {
            Some(source_from_tag(bytes[9]).ok_or(DecodeError::FaultTag { tag: bytes[9] })?)
        };
        let outcome = match (kind, fault) {
            (OUTCOME_COMPLETE, None) => Outcome::Complete,
            (OUTCOME_COMPLETE, Some(source)) => {
                return Err(DecodeError::UnexpectedFault { source });
            }
            (_, None) => return Err(DecodeError::MissingFault),
            (_, Some(source)) => {
                if !sources.contains(source) {
                    return Err(DecodeError::FaultOutsideSources { source });
                }
                Outcome::Incomplete(match kind {
                    OUTCOME_CAPTURE => Fault::Capture(source),
                    OUTCOME_DEVICE => Fault::Device(source),
                    OUTCOME_ENGINE => Fault::Engine(source),
                    _ => Fault::Permission(source),
                })
            }
        };
        Ok(Self {
            stage,
            sources,
            progress: Progress {
                transcript,
                outcome,
            },
        })
    }
}

impl Session {
    /// Rebuilds a session at the settled stage a checkpoint names, using the
    /// permissions the operating system reports now.
    ///
    /// The result is never preparing, recording, or closing, so it has no open,
    /// pending, or finishing source.
    #[must_use]
    pub const fn restore(checkpoint: Checkpoint, permissions: Permissions) -> Self {
        let progress = checkpoint.progress;
        let sources = checkpoint.sources;
        let phase = match checkpoint.stage {
            Settled::Paused => Phase::Paused(Paused { sources, progress }),
            Settled::Reviewing => Phase::Reviewing(Reviewing { sources, progress }),
            Settled::Confirmed => Phase::Confirmed(Confirmed { sources, progress }),
        };
        Self { permissions, phase }
    }
}

/// Why a byte image is not a checkpoint this crate wrote.
#[non_exhaustive]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DecodeError {
    /// The image is not [`CHECKPOINT_LEN`] bytes long.
    Length {
        /// Length of the image.
        len: usize,
    },
    /// The image does not start with the checkpoint magic.
    Magic {
        /// The four bytes found instead.
        found: [u8; 4],
    },
    /// The image names a format version this crate does not know.
    Version {
        /// The version byte found.
        found: u8,
    },
    /// The trailing checksum does not cover the leading bytes.
    Checksum {
        /// Checksum of the leading bytes.
        expected: u32,
        /// Checksum stored in the image.
        found: u32,
    },
    /// The stage byte names no settled stage.
    StageTag {
        /// The stage byte found.
        tag: u8,
    },
    /// The source mask is empty or names a bit outside the two sources.
    Sources {
        /// The source mask found.
        mask: u8,
    },
    /// The committed mask names a bit outside the two sources.
    CommittedMask {
        /// The committed mask found.
        mask: u8,
    },
    /// The committed mask names a source the meeting does not record.
    CommittedOutsideSources {
        /// The source that is not part of the meeting.
        source: Source,
    },
    /// The outcome byte names no outcome kind.
    OutcomeKind {
        /// The outcome byte found.
        tag: u8,
    },
    /// A complete outcome also names a fault.
    UnexpectedFault {
        /// The fault source found on a complete outcome.
        source: Source,
    },
    /// An incomplete outcome names no fault.
    MissingFault,
    /// The fault source byte names no source and is not the no-fault marker.
    FaultTag {
        /// The fault source byte found.
        tag: u8,
    },
    /// The fault names a source the meeting does not record.
    FaultOutsideSources {
        /// The source that is not part of the meeting.
        source: Source,
    },
}

impl fmt::Display for DecodeError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Length { len } => {
                write!(
                    formatter,
                    "a checkpoint is {CHECKPOINT_LEN} bytes, not {len}"
                )
            }
            Self::Magic { found } => {
                write!(formatter, "a checkpoint starts with KOES, not {found:?}")
            }
            Self::Version { found } => write!(formatter, "checkpoint version {found} is unknown"),
            Self::Checksum { expected, found } => write!(
                formatter,
                "checksum {found:#010x} does not cover the bytes; expected {expected:#010x}"
            ),
            Self::StageTag { tag } => write!(formatter, "checkpoint stage tag {tag} is unknown"),
            Self::Sources { mask } => {
                write!(
                    formatter,
                    "checkpoint source mask {mask:#04x} is not a meeting"
                )
            }
            Self::CommittedMask { mask } => {
                write!(
                    formatter,
                    "checkpoint committed mask {mask:#04x} names no source"
                )
            }
            Self::CommittedOutsideSources { source } => write!(
                formatter,
                "committed text names {source}, which the meeting does not record"
            ),
            Self::OutcomeKind { tag } => {
                write!(formatter, "checkpoint outcome tag {tag} is unknown")
            }
            Self::UnexpectedFault { source } => {
                write!(formatter, "a complete outcome carries {source} as a fault")
            }
            Self::MissingFault => formatter.write_str("an incomplete outcome names no fault"),
            Self::FaultTag { tag } => write!(formatter, "checkpoint fault tag {tag} is unknown"),
            Self::FaultOutsideSources { source } => {
                write!(formatter, "{source} fault is outside the meeting's sources")
            }
        }
    }
}

impl core::error::Error for DecodeError {}

const fn settled(progress: Progress) -> Progress {
    let mut transcript = progress.transcript;
    transcript.set_volatile(Source::Microphone, false);
    transcript.set_volatile(Source::SystemAudio, false);
    Progress {
        transcript,
        outcome: progress.outcome,
    }
}

const fn committed_bits(transcript: Transcript) -> u8 {
    let mut bits = 0;
    if transcript.committed(Source::Microphone) {
        bits |= SourceSet::bit(Source::Microphone);
    }
    if transcript.committed(Source::SystemAudio) {
        bits |= SourceSet::bit(Source::SystemAudio);
    }
    bits
}

const fn outcome_tag(outcome: Outcome) -> u8 {
    match outcome {
        Outcome::Complete => OUTCOME_COMPLETE,
        Outcome::Incomplete(Fault::Capture(_)) => OUTCOME_CAPTURE,
        Outcome::Incomplete(Fault::Device(_)) => OUTCOME_DEVICE,
        Outcome::Incomplete(Fault::Engine(_)) => OUTCOME_ENGINE,
        Outcome::Incomplete(Fault::Permission(_)) => OUTCOME_PERMISSION,
    }
}

const fn fault_source(fault: Fault) -> Source {
    match fault {
        Fault::Capture(source)
        | Fault::Device(source)
        | Fault::Engine(source)
        | Fault::Permission(source) => source,
    }
}

const fn source_tag(source: Source) -> u8 {
    match source {
        Source::Microphone => 0,
        Source::SystemAudio => 1,
    }
}

const fn source_from_tag(tag: u8) -> Option<Source> {
    match tag {
        0 => Some(Source::Microphone),
        1 => Some(Source::SystemAudio),
        _ => None,
    }
}

fn crc32(bytes: &[u8]) -> u32 {
    let mut crc = 0xFFFF_FFFFu32;
    for &byte in bytes {
        crc ^= u32::from(byte);
        for _ in 0..8 {
            crc = if crc & 1 == 0 {
                crc >> 1
            } else {
                (crc >> 1) ^ 0xEDB8_8320
            };
        }
    }
    !crc
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;
    use crate::input::Input;
    use crate::permission::Permission;
    use crate::sources::SOURCES;
    use crate::state::Stage;
    use crate::transcript::{SegmentOutcome, SegmentResult};

    fn granted() -> Permissions {
        Permissions::new(Permission::Granted, Permission::Granted)
    }

    fn drive(session: Session, inputs: &[Input]) -> Session {
        inputs.iter().fold(session, |session, input| {
            session.step(*input).unwrap().session
        })
    }

    fn session_of(inputs: &[Input]) -> Session {
        drive(Session::new(granted()), inputs)
    }

    fn progress_with(committed: [bool; 2], outcome: Outcome) -> Progress {
        let mut transcript = Transcript::new();
        transcript.set_committed(Source::Microphone, committed[0]);
        transcript.set_committed(Source::SystemAudio, committed[1]);
        Progress {
            transcript,
            outcome,
        }
    }

    fn checkpoint(stage: Settled, sources: SourceSet, progress: Progress) -> Checkpoint {
        Checkpoint {
            stage,
            sources,
            progress,
        }
    }

    fn recording_one() -> Session {
        session_of(&[
            Input::Start(SourceSet::MICROPHONE),
            Input::DeviceOpened(Source::Microphone),
        ])
    }

    fn paused_one() -> Session {
        drive(
            recording_one(),
            &[
                Input::Pause,
                Input::SegmentClosed {
                    source: Source::Microphone,
                    result: SegmentResult {
                        outcome: SegmentOutcome::Complete,
                        committed: true,
                    },
                },
            ],
        )
    }

    fn recording_with_volatile() -> Session {
        session_of(&[
            Input::Start(SourceSet::MEETING),
            Input::DeviceOpened(Source::Microphone),
            Input::DeviceOpened(Source::SystemAudio),
            Input::EngineCommitted(Source::Microphone),
            Input::EngineVolatile(Source::Microphone),
            Input::CaptureFailed(Source::SystemAudio),
        ])
    }

    fn reviewing() -> Session {
        drive(
            recording_one(),
            &[
                Input::Stop,
                Input::SegmentClosed {
                    source: Source::Microphone,
                    result: SegmentResult {
                        outcome: SegmentOutcome::Complete,
                        committed: true,
                    },
                },
            ],
        )
    }

    fn confirmed() -> Session {
        drive(reviewing(), &[Input::Confirm])
    }

    fn golden() -> Checkpoint {
        checkpoint(
            Settled::Paused,
            SourceSet::MEETING,
            progress_with(
                [true, false],
                Outcome::Incomplete(Fault::Capture(Source::SystemAudio)),
            ),
        )
    }

    fn mic_image() -> [u8; CHECKPOINT_LEN] {
        checkpoint(
            Settled::Paused,
            SourceSet::MICROPHONE,
            progress_with([false, false], Outcome::Complete),
        )
        .to_bytes()
    }

    fn sealed(mut bytes: [u8; CHECKPOINT_LEN]) -> [u8; CHECKPOINT_LEN] {
        let checksum = crc32(&bytes[..10]).to_le_bytes();
        bytes[10..14].copy_from_slice(&checksum);
        bytes
    }

    fn checksum_of(bytes: &[u8; CHECKPOINT_LEN]) -> u32 {
        u32::from_le_bytes([bytes[10], bytes[11], bytes[12], bytes[13]])
    }

    #[test]
    fn the_crc_check_value_is_the_standard_one() {
        assert_eq!(crc32(b"123456789"), 0xCBF4_3926);
    }

    #[test]
    fn the_golden_image_is_stable() {
        let bytes = golden().to_bytes();
        assert_eq!(
            bytes,
            [
                0x4B, 0x4F, 0x45, 0x53, 1, 1, 3, 1, 1, 1, 0x4F, 0x59, 0xEC, 0x7C
            ]
        );
        assert_eq!(Checkpoint::from_bytes(&bytes), Ok(golden()));
    }

    #[test]
    fn an_idle_session_has_no_checkpoint() {
        assert_eq!(Checkpoint::of(&Session::default()), None);
        assert_eq!(Checkpoint::of(&Session::new(granted())), None);
    }

    #[test]
    fn a_fresh_preparation_has_no_checkpoint() {
        let preparing = session_of(&[Input::Start(SourceSet::MEETING)]);
        assert_eq!(preparing.stage(), Stage::Preparing);
        assert_eq!(Checkpoint::of(&preparing), None);
    }

    #[test]
    fn a_resumed_preparation_settles_back_to_paused() {
        let preparing = drive(paused_one(), &[Input::Resume]);
        assert_eq!(preparing.stage(), Stage::Preparing);
        assert_eq!(
            Checkpoint::of(&preparing),
            Some(checkpoint(
                Settled::Paused,
                SourceSet::MICROPHONE,
                progress_with([true, false], Outcome::Complete),
            ))
        );
    }

    #[test]
    fn a_recording_settles_to_paused_and_drops_volatile_text() {
        let session = recording_with_volatile();
        assert_eq!(session.stage(), Stage::Recording);
        assert_eq!(
            Checkpoint::of(&session),
            Some(checkpoint(
                Settled::Paused,
                SourceSet::MEETING,
                progress_with(
                    [true, false],
                    Outcome::Incomplete(Fault::Capture(Source::SystemAudio)),
                ),
            ))
        );
    }

    #[test]
    fn a_paused_session_keeps_its_stage() {
        assert_eq!(
            Checkpoint::of(&paused_one()),
            Some(checkpoint(
                Settled::Paused,
                SourceSet::MICROPHONE,
                progress_with([true, false], Outcome::Complete),
            ))
        );
    }

    #[test]
    fn closing_toward_pause_settles_to_paused() {
        let closing = drive(recording_with_volatile(), &[Input::Pause]);
        assert_eq!(closing.stage(), Stage::Closing);
        assert_eq!(
            Checkpoint::of(&closing),
            Some(checkpoint(
                Settled::Paused,
                SourceSet::MEETING,
                progress_with(
                    [true, false],
                    Outcome::Incomplete(Fault::Capture(Source::SystemAudio)),
                ),
            ))
        );
    }

    #[test]
    fn closing_toward_stop_settles_to_reviewing() {
        let closing = drive(recording_one(), &[Input::Stop]);
        assert_eq!(closing.stage(), Stage::Closing);
        assert_eq!(
            Checkpoint::of(&closing),
            Some(checkpoint(
                Settled::Reviewing,
                SourceSet::MICROPHONE,
                progress_with([false, false], Outcome::Complete),
            ))
        );
    }

    #[test]
    fn a_reviewing_session_keeps_its_stage() {
        assert_eq!(
            Checkpoint::of(&reviewing()),
            Some(checkpoint(
                Settled::Reviewing,
                SourceSet::MICROPHONE,
                progress_with([true, false], Outcome::Complete),
            ))
        );
    }

    #[test]
    fn a_confirmed_session_keeps_its_stage() {
        assert_eq!(
            Checkpoint::of(&confirmed()),
            Some(checkpoint(
                Settled::Confirmed,
                SourceSet::MICROPHONE,
                progress_with([true, false], Outcome::Complete),
            ))
        );
    }

    #[test]
    fn restoring_uses_the_given_permissions() {
        let permissions = Permissions::new(Permission::Granted, Permission::Denied);
        let restored = Session::restore(golden(), permissions);
        assert_eq!(restored.permissions(), permissions);
        assert_eq!(restored.permission(Source::Microphone), Permission::Granted);
        assert_eq!(restored.permission(Source::SystemAudio), Permission::Denied);
        assert_eq!(restored.stage(), Stage::Paused);
        assert_eq!(restored.sources(), Some(SourceSet::MEETING));
        assert_eq!(
            restored.transcript(),
            Some(progress_with([true, false], Outcome::Complete).transcript)
        );
        assert_eq!(
            restored.outcome(),
            Some(Outcome::Incomplete(Fault::Capture(Source::SystemAudio)))
        );
    }

    #[test]
    fn a_restored_session_reports_no_live_sources() {
        let checkpoints = [
            golden(),
            Checkpoint::of(&reviewing()).unwrap(),
            Checkpoint::of(&confirmed()).unwrap(),
        ];
        for checkpoint in checkpoints {
            let restored = Session::restore(checkpoint, granted());
            assert_eq!(restored.open_sources(), SourceSet::EMPTY);
            assert_eq!(restored.pending_sources(), SourceSet::EMPTY);
            assert_eq!(restored.finishing_sources(), SourceSet::EMPTY);
        }
    }

    #[test]
    fn every_valid_checkpoint_round_trips() {
        for stage in [Settled::Paused, Settled::Reviewing, Settled::Confirmed] {
            for sources in [
                SourceSet::MICROPHONE,
                SourceSet::SYSTEM_AUDIO,
                SourceSet::MEETING,
            ] {
                for committed in 0u8..=3 {
                    if committed & !sources.bits() != 0 {
                        continue;
                    }
                    let mut outcomes = vec![Outcome::Complete];
                    for source in sources.iter() {
                        outcomes.push(Outcome::Incomplete(Fault::Capture(source)));
                        outcomes.push(Outcome::Incomplete(Fault::Device(source)));
                        outcomes.push(Outcome::Incomplete(Fault::Engine(source)));
                        outcomes.push(Outcome::Incomplete(Fault::Permission(source)));
                    }
                    for outcome in outcomes {
                        let checkpoint = checkpoint(
                            stage,
                            sources,
                            Progress {
                                transcript: transcript_of(committed),
                                outcome,
                            },
                        );
                        assert_eq!(
                            Checkpoint::from_bytes(&checkpoint.to_bytes()),
                            Ok(checkpoint),
                            "{stage:?} {sources:?} {committed} {outcome:?}"
                        );
                    }
                }
            }
        }
    }

    fn transcript_of(committed: u8) -> Transcript {
        let mut transcript = Transcript::new();
        for source in SOURCES {
            if committed & SourceSet::bit(source) != 0 {
                transcript.set_committed(source, true);
            }
        }
        transcript
    }

    #[test]
    fn every_single_byte_corruption_is_rejected() {
        let image = golden().to_bytes();
        for offset in 0..CHECKPOINT_LEN {
            for value in 0u8..=u8::MAX {
                if value == image[offset] {
                    continue;
                }
                let mut corrupt = image;
                corrupt[offset] = value;
                assert!(
                    Checkpoint::from_bytes(&corrupt).is_err(),
                    "offset {offset} value {value}"
                );
            }
        }
    }

    #[test]
    fn every_wrong_length_is_rejected() {
        let image = golden().to_bytes();
        let mut padded = [0u8; CHECKPOINT_LEN + 6];
        padded[..CHECKPOINT_LEN].copy_from_slice(&image);
        for len in (0..CHECKPOINT_LEN).chain(CHECKPOINT_LEN + 1..=CHECKPOINT_LEN + 6) {
            assert_eq!(
                Checkpoint::from_bytes(&padded[..len]),
                Err(DecodeError::Length { len })
            );
        }
    }

    #[test]
    fn a_bad_magic_is_reported() {
        let mut bytes = mic_image();
        bytes[0] = b'X';
        assert_eq!(
            Checkpoint::from_bytes(&sealed(bytes)),
            Err(DecodeError::Magic { found: *b"XOES" })
        );
    }

    #[test]
    fn an_unknown_version_is_reported() {
        let mut bytes = mic_image();
        bytes[4] = 2;
        assert_eq!(
            Checkpoint::from_bytes(&sealed(bytes)),
            Err(DecodeError::Version { found: 2 })
        );
    }

    #[test]
    fn a_checksum_mismatch_is_reported() {
        let mut bytes = mic_image();
        bytes[10] ^= 0xFF;
        assert_eq!(
            Checkpoint::from_bytes(&bytes),
            Err(DecodeError::Checksum {
                expected: crc32(&bytes[..10]),
                found: checksum_of(&bytes),
            })
        );
    }

    #[test]
    fn an_unknown_stage_tag_is_reported() {
        let mut bytes = mic_image();
        bytes[5] = 4;
        assert_eq!(
            Checkpoint::from_bytes(&sealed(bytes)),
            Err(DecodeError::StageTag { tag: 4 })
        );
    }

    #[test]
    fn an_empty_source_mask_is_reported() {
        let mut bytes = mic_image();
        bytes[6] = 0;
        assert_eq!(
            Checkpoint::from_bytes(&sealed(bytes)),
            Err(DecodeError::Sources { mask: 0 })
        );
    }

    #[test]
    fn an_out_of_range_source_mask_is_reported() {
        let mut bytes = mic_image();
        bytes[6] = 4;
        assert_eq!(
            Checkpoint::from_bytes(&sealed(bytes)),
            Err(DecodeError::Sources { mask: 4 })
        );
    }

    #[test]
    fn a_committed_mask_with_unknown_bits_is_reported() {
        let mut bytes = mic_image();
        bytes[7] = 4;
        assert_eq!(
            Checkpoint::from_bytes(&sealed(bytes)),
            Err(DecodeError::CommittedMask { mask: 4 })
        );
    }

    #[test]
    fn committed_text_outside_the_meeting_is_reported() {
        let mut bytes = mic_image();
        bytes[7] = SourceSet::bit(Source::SystemAudio);
        assert_eq!(
            Checkpoint::from_bytes(&sealed(bytes)),
            Err(DecodeError::CommittedOutsideSources {
                source: Source::SystemAudio
            })
        );
    }

    #[test]
    fn an_unknown_outcome_kind_is_reported() {
        let mut bytes = mic_image();
        bytes[8] = 5;
        assert_eq!(
            Checkpoint::from_bytes(&sealed(bytes)),
            Err(DecodeError::OutcomeKind { tag: 5 })
        );
    }

    #[test]
    fn a_fault_on_a_complete_outcome_is_reported() {
        let mut bytes = mic_image();
        bytes[9] = source_tag(Source::Microphone);
        assert_eq!(
            Checkpoint::from_bytes(&sealed(bytes)),
            Err(DecodeError::UnexpectedFault {
                source: Source::Microphone
            })
        );
    }

    #[test]
    fn an_incomplete_outcome_without_a_fault_is_reported() {
        let mut bytes = mic_image();
        bytes[8] = OUTCOME_CAPTURE;
        bytes[9] = source_tag(Source::Microphone);
        let bytes = sealed(bytes);
        assert_eq!(
            Checkpoint::from_bytes(&bytes),
            Ok(checkpoint(
                Settled::Paused,
                SourceSet::MICROPHONE,
                progress_with(
                    [false, false],
                    Outcome::Incomplete(Fault::Capture(Source::Microphone)),
                ),
            ))
        );
        let mut missing = bytes;
        missing[9] = NO_FAULT;
        assert_eq!(
            Checkpoint::from_bytes(&sealed(missing)),
            Err(DecodeError::MissingFault)
        );
    }

    #[test]
    fn an_unknown_fault_source_tag_is_reported() {
        let mut bytes = mic_image();
        bytes[8] = OUTCOME_CAPTURE;
        bytes[9] = 2;
        assert_eq!(
            Checkpoint::from_bytes(&sealed(bytes)),
            Err(DecodeError::FaultTag { tag: 2 })
        );
    }

    #[test]
    fn a_fault_outside_the_meeting_is_reported() {
        let mut bytes = mic_image();
        bytes[8] = OUTCOME_CAPTURE;
        bytes[9] = source_tag(Source::SystemAudio);
        assert_eq!(
            Checkpoint::from_bytes(&sealed(bytes)),
            Err(DecodeError::FaultOutsideSources {
                source: Source::SystemAudio
            })
        );
    }

    #[test]
    fn display_names_every_decode_error() {
        let cases = [
            (
                DecodeError::Length { len: 3 },
                "a checkpoint is 14 bytes, not 3",
            ),
            (
                DecodeError::Magic { found: *b"XOES" },
                "a checkpoint starts with KOES, not [88, 79, 69, 83]",
            ),
            (
                DecodeError::Version { found: 2 },
                "checkpoint version 2 is unknown",
            ),
            (
                DecodeError::Checksum {
                    expected: 0x1234_5678,
                    found: 0x9ABC_DEF0,
                },
                "checksum 0x9abcdef0 does not cover the bytes; expected 0x12345678",
            ),
            (
                DecodeError::StageTag { tag: 4 },
                "checkpoint stage tag 4 is unknown",
            ),
            (
                DecodeError::Sources { mask: 4 },
                "checkpoint source mask 0x04 is not a meeting",
            ),
            (
                DecodeError::CommittedMask { mask: 4 },
                "checkpoint committed mask 0x04 names no source",
            ),
            (
                DecodeError::CommittedOutsideSources {
                    source: Source::SystemAudio,
                },
                "committed text names system audio, which the meeting does not record",
            ),
            (
                DecodeError::OutcomeKind { tag: 5 },
                "checkpoint outcome tag 5 is unknown",
            ),
            (
                DecodeError::UnexpectedFault {
                    source: Source::Microphone,
                },
                "a complete outcome carries microphone as a fault",
            ),
            (
                DecodeError::MissingFault,
                "an incomplete outcome names no fault",
            ),
            (
                DecodeError::FaultTag { tag: 2 },
                "checkpoint fault tag 2 is unknown",
            ),
            (
                DecodeError::FaultOutsideSources {
                    source: Source::SystemAudio,
                },
                "system audio fault is outside the meeting's sources",
            ),
        ];
        for (error, text) in cases {
            assert_eq!(error.to_string(), text);
        }
    }
}
