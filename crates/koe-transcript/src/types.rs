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

impl From<Uuid> for SegmentId {
    fn from(id: Uuid) -> Self {
        Self(id)
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

/// Semantic reason a transcript segment cannot be persisted.
#[derive(Clone, Copy, Debug, Error, Eq, PartialEq)]
#[non_exhaustive]
pub enum TranscriptValidationError {
    #[error("unsupported transcript schema version")]
    SchemaVersion,
    #[error("segment timestamps are reversed")]
    Timestamps,
    #[error("segment source is empty or contains control characters")]
    Source,
    #[error("model identity is empty or contains control characters")]
    ModelIdentity,
}

impl TranscriptSegment {
    /// Validates the invariants required by transcript persistence.
    ///
    /// # Errors
    ///
    /// Returns a structured reason for the first invalid field group.
    pub fn validate(&self) -> Result<(), TranscriptValidationError> {
        if self.schema_version != 1 {
            return Err(TranscriptValidationError::SchemaVersion);
        }
        if self.start_ms > self.end_ms {
            return Err(TranscriptValidationError::Timestamps);
        }
        if self.source.is_empty() || self.source.chars().any(char::is_control) {
            return Err(TranscriptValidationError::Source);
        }
        if self.model.as_ref().is_some_and(|model| {
            [&model.id, &model.version, &model.variant]
                .into_iter()
                .any(|value| value.is_empty() || value.chars().any(char::is_control))
        }) {
            return Err(TranscriptValidationError::ModelIdentity);
        }
        Ok(())
    }

    /// Creates and validates a final segment with the schema version stamp.
    ///
    /// # Errors
    ///
    /// Returns the first semantic validation failure.
    pub fn final_segment(
        start_ms: u64,
        end_ms: u64,
        text: String,
        model: Option<TranscriptModel>,
        audio_discontinuities: Vec<u64>,
    ) -> Result<Self, TranscriptValidationError> {
        let segment = Self {
            schema_version: 1,
            segment_id: SegmentId::new(),
            source: "mixed".to_owned(),
            start_ms,
            end_ms,
            text,
            is_final: true,
            model,
            audio_discontinuities,
        };
        segment.validate()?;
        Ok(segment)
    }

    /// Creates and validates a new revision of the same segment.
    ///
    /// # Errors
    ///
    /// Returns the first semantic validation failure.
    pub fn revise(
        &self,
        end_ms: u64,
        text: String,
        is_final: bool,
        audio_discontinuities: Vec<u64>,
    ) -> Result<Self, TranscriptValidationError> {
        let segment = Self {
            schema_version: self.schema_version,
            segment_id: self.segment_id,
            source: self.source.clone(),
            start_ms: self.start_ms,
            end_ms,
            text,
            is_final,
            model: self.model.clone(),
            audio_discontinuities,
        };
        segment.validate()?;
        Ok(segment)
    }
}

/// Stable transcript failures without content or paths.
#[derive(Clone, Copy, Debug, Error, Eq, PartialEq)]
#[non_exhaustive]
pub enum TranscriptError {
    #[error("the transcript path was rejected")]
    PathRejected,
    #[error("the transcript could not be written")]
    WriteFailed,
    #[error("the transcript segment exceeds the record-size limit")]
    RecordTooLarge,
    #[error("the transcript segment is semantically invalid: {0}")]
    InvalidSegment(#[from] TranscriptValidationError),
    #[error("the transcript store is already open by another owner")]
    StoreLocked,
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
            Self::RecordTooLarge => "KOE-TRANSCRIPT-RECORD-TOO-LARGE",
            Self::InvalidSegment(_) => "KOE-TRANSCRIPT-INVALID-SEGMENT",
            Self::StoreLocked => "KOE-TRANSCRIPT-STORE-LOCKED",
            Self::CorruptLog => "KOE-TRANSCRIPT-CORRUPT-LOG",
            Self::WriteFailed | Self::AlreadyFinalized => "KOE-STORE-FINALIZE-FAILED",
        }
    }
}
