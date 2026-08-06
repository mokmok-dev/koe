//! Transcript segment types and stable errors.

use std::fmt;

use serde::{Deserialize, Serialize};
use thiserror::Error;
use uuid::Uuid;

/// Stable koe-generated segment identifier.
#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(transparent)]
pub struct SegmentId(Uuid);

impl SegmentId {
    /// Creates a new segment identifier.
    #[must_use]
    pub fn new() -> Self {
        Self(Uuid::new_v4())
    }
}

impl Default for SegmentId {
    fn default() -> Self {
        Self::new()
    }
}

impl fmt::Display for SegmentId {
    fn fmt(
        &self,
        formatter: &mut fmt::Formatter<'_>,
    ) -> fmt::Result {
        self.0.fmt(formatter)
    }
}

/// Model metadata recorded with every segment.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct TranscriptModel {
    pub id: String,
    pub version: String,
    pub variant: String,
}

/// One transcript segment, schema version 1.
///
/// Times are session monotonics in milliseconds per
/// `spec/04-storage-and-transcripts.md`. Interim revisions reuse
/// `segment_id`; they are appended, never replaced in the log.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct TranscriptSegment {
    pub schema_version: u32,
    pub segment_id: SegmentId,
    pub source: String,
    pub start_ms: u64,
    pub end_ms: u64,
    pub text: String,
    #[serde(rename = "final")]
    pub is_final: bool,
    pub model: Option<TranscriptModel>,
    /// Session-clock discontinuities overlapping this segment, in µs.
    #[serde(default)]
    pub audio_discontinuities: Vec<u64>,
}

impl TranscriptSegment {
    /// Creates a final segment with the schema version stamp.
    #[must_use]
    pub fn final_segment(
        start_ms: u64,
        end_ms: u64,
        text: String,
        model: Option<TranscriptModel>,
        audio_discontinuities: Vec<u64>,
    ) -> Self {
        Self {
            schema_version: 1,
            segment_id: SegmentId::new(),
            source: "mixed".to_owned(),
            start_ms,
            end_ms,
            text,
            is_final: true,
            model,
            audio_discontinuities,
        }
    }

    /// Replaces this revision with a new revision of the same segment.
    #[must_use]
    pub fn revise(
        &self,
        end_ms: u64,
        text: String,
        is_final: bool,
        audio_discontinuities: Vec<u64>,
    ) -> Self {
        Self {
            schema_version: self.schema_version,
            segment_id: self.segment_id,
            source: self.source.clone(),
            start_ms: self.start_ms,
            end_ms,
            text,
            is_final,
            model: self.model.clone(),
            audio_discontinuities,
        }
    }
}

/// Stable transcript failures without content or paths.
#[derive(Clone, Copy, Debug, Error, Eq, PartialEq)]
pub enum TranscriptError {
    #[error("the transcript path was rejected")]
    PathRejected,
    #[error("the transcript could not be written")]
    WriteFailed,
    #[error("the transcript log is inconsistent")]
    CorruptLog,
    #[error("the transcript store was already finalized")]
    AlreadyFinalized,
}

impl TranscriptError {
    /// Stable presentation code.
    #[must_use]
    pub const fn code(self) -> &'static str {
        match self {
            Self::PathRejected => "KOE-STORE-PATH-REJECTED",
            Self::WriteFailed | Self::CorruptLog | Self::AlreadyFinalized => {
                "KOE-STORE-FINALIZE-FAILED"
            },
        }
    }
}
