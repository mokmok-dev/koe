//! Why a store operation or a stored record was rejected.

use std::fmt;
use std::io;

use koe_capture_core::Source;

use crate::record::{
    CREATE_ATTEMPTS, HEADER_LEN, MAX_RECORD_LEN, MAX_TEXT_BYTES, SECTION_HEADER_LEN,
    SEGMENT_HEADER_LEN, TRAILER_LEN,
};

/// Why a store operation failed.
#[non_exhaustive]
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Error {
    /// A file operation failed.
    Io {
        /// Operation that failed.
        operation: &'static str,
        /// Native error kind.
        kind: io::ErrorKind,
    },
    /// The record holds bytes that are not a record this build wrote.
    Decode(DecodeError),
    /// Every id near the requested millisecond is taken.
    IdCollision,
    /// A transcript does not fit the space a record gives it.
    TextTooLarge {
        /// Source whose transcript is too large.
        source: Source,
        /// Encoded length of that transcript's section.
        bytes: usize,
    },
    /// The record's parts add up to more than the format can describe.
    RecordTooLarge {
        /// Length the parts add up to.
        bytes: usize,
    },
    /// The name is not one plain file name inside the meeting directory, or it
    /// names a file the store writes itself.
    InvalidRecording,
    /// The record holds the transcripts, but the text rendered beside it does
    /// not.
    TextNotWritten {
        /// Failure writing `transcript.txt`.
        source: Box<Self>,
    },
}

/// Maps a failed file operation to [`Error::Io`].
pub fn io_error(operation: &'static str) -> impl FnOnce(io::Error) -> Error {
    move |error| Error::Io {
        operation,
        kind: error.kind(),
    }
}

impl fmt::Display for Error {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io { operation, kind } => {
                write!(formatter, "session store `{operation}` failed: {kind}")
            }
            Self::Decode(error) => write!(
                formatter,
                "session record is not a record this build wrote: {error}"
            ),
            Self::IdCollision => write!(
                formatter,
                "every meeting id within {CREATE_ATTEMPTS} milliseconds of the requested time is taken"
            ),
            Self::TextTooLarge { source, bytes } => write!(
                formatter,
                "{source} transcript does not fit a record: {bytes} bytes; a record holds at most {MAX_TEXT_BYTES}"
            ),
            Self::RecordTooLarge { bytes } => write!(
                formatter,
                "a record of {bytes} bytes cannot be written; the format holds at most {MAX_RECORD_LEN}"
            ),
            Self::InvalidRecording => formatter.write_str(
                "a recording name must be one plain file name in the meeting directory that the store does not write itself",
            ),
            Self::TextNotWritten { source } => {
                write!(formatter, "transcript.txt was not written: {source}")
            }
        }
    }
}

impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Decode(error) => Some(error),
            Self::TextNotWritten { source } => Some(source.as_ref()),
            _ => None,
        }
    }
}

/// Why a byte image is not a record this build wrote.
#[non_exhaustive]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DecodeError {
    /// The image is longer than any record this build writes.
    TooLong {
        /// Length of the image.
        len: usize,
    },
    /// The image is too short to hold a header, a checkpoint, and a checksum.
    Length {
        /// Length of the image.
        len: usize,
    },
    /// The image does not start with the record magic.
    Magic {
        /// The four bytes found instead.
        found: [u8; 4],
    },
    /// The image names a record version this build does not know.
    Version {
        /// The version byte found.
        found: u8,
    },
    /// The flags byte carries a bit this build does not define.
    Flags {
        /// The flags byte found.
        flags: u8,
    },
    /// A reserved byte is not zero.
    Reserved {
        /// Offset of the first reserved byte that is not zero.
        offset: usize,
    },
    /// The trailing checksum does not cover the leading bytes.
    Checksum {
        /// Checksum of the leading bytes.
        expected: u32,
        /// Checksum stored in the image.
        found: u32,
    },
    /// The stored total length disagrees with the bytes the record holds.
    TotalLength {
        /// Total length stored in the header.
        declared: u32,
        /// Length the image or its header fields add up to.
        actual: usize,
    },
    /// The checkpoint is not one `koe-session` wrote.
    Checkpoint(koe_session::DecodeError),
    /// The checkpoint flag is clear but the checkpoint bytes are not zero.
    UnexpectedCheckpoint,
    /// The recording flag is clear but a name length is stored.
    UnexpectedRecording,
    /// The recording name is not one plain file name.
    RecordingName {
        /// Length of the name in the record.
        len: u16,
    },
    /// The recording name is one the store reserves for a file it writes.
    ReservedName {
        /// Length of the name in the record.
        len: u16,
    },
    /// A text section's length is not one a transcript can occupy.
    SectionLength {
        /// Source the section belongs to.
        source: Source,
        /// Stored section length.
        len: u32,
    },
    /// A text section is too short for the segment count it stores.
    SegmentCount {
        /// Source the section belongs to.
        source: Source,
        /// Stored segment count.
        count: u32,
        /// Stored section length.
        len: u32,
    },
    /// A segment runs past the end of its section.
    Segment {
        /// Source the section belongs to.
        source: Source,
        /// Index of the segment inside the section.
        index: u32,
    },
    /// A segment's text is not valid UTF-8.
    Text {
        /// Source the section belongs to.
        source: Source,
        /// Index of the segment inside the section.
        index: u32,
    },
    /// A text section has bytes left after its last segment.
    Section {
        /// Source the section belongs to.
        source: Source,
    },
}

impl fmt::Display for DecodeError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::TooLong { len } => {
                write!(
                    formatter,
                    "a record is at most {MAX_RECORD_LEN} bytes, not {len}"
                )
            }
            Self::Length { len } => write!(
                formatter,
                "a record is at least {} bytes, not {len}",
                HEADER_LEN + koe_session::CHECKPOINT_LEN + TRAILER_LEN
            ),
            Self::Magic { found } => write!(formatter, "a record starts with KOEM, not {found:?}"),
            Self::Version { found } => write!(formatter, "record version {found} is unknown"),
            Self::Flags { flags } => {
                write!(
                    formatter,
                    "record flags {flags:#04x} carry bits this build does not write"
                )
            }
            Self::Reserved { offset } => {
                write!(
                    formatter,
                    "record byte {offset} is reserved and must be zero"
                )
            }
            Self::Checksum { expected, found } => write!(
                formatter,
                "checksum {found:#010x} does not cover the bytes; expected {expected:#010x}"
            ),
            Self::TotalLength { declared, actual } => {
                write!(
                    formatter,
                    "a record declares {declared} bytes but holds {actual}"
                )
            }
            Self::Checkpoint(error) => {
                write!(
                    formatter,
                    "the checkpoint is not one this build wrote: {error}"
                )
            }
            Self::UnexpectedCheckpoint => {
                formatter.write_str("a record without a checkpoint carries checkpoint bytes")
            }
            Self::UnexpectedRecording => {
                formatter.write_str("a record without a recording carries an audio file name")
            }
            Self::RecordingName { len } => write!(
                formatter,
                "a recording name of {len} bytes is not one plain file name"
            ),
            Self::ReservedName { len } => write!(
                formatter,
                "a recording name of {len} bytes is reserved for a file the store writes itself"
            ),
            Self::SectionLength { source, len } => write!(
                formatter,
                "{source} transcript section of {len} bytes is not {SECTION_HEADER_LEN}..={MAX_TEXT_BYTES}"
            ),
            Self::SegmentCount { source, count, len } => write!(
                formatter,
                "{source} transcript section of {len} bytes cannot hold {count} segments of {SEGMENT_HEADER_LEN} bytes"
            ),
            Self::Segment { source, index } => {
                write!(
                    formatter,
                    "{source} transcript segment {index} runs past its section"
                )
            }
            Self::Text { source, index } => {
                write!(
                    formatter,
                    "{source} transcript segment {index} is not UTF-8 text"
                )
            }
            Self::Section { source } => {
                write!(formatter, "{source} transcript section has bytes left over")
            }
        }
    }
}

impl std::error::Error for DecodeError {}
