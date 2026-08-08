//! Transcript segment types and stable errors.

use std::fmt;

use serde::{Deserialize, Serialize};
use thiserror::Error;
use uuid::Uuid;

/// The transcript schema version emitted by this crate.
pub const TRANSCRIPT_SCHEMA_VERSION: u32 = 1;

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

    /// Returns the underlying UUID.
    #[must_use]
    pub const fn as_uuid(&self) -> &Uuid {
        &self.0
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
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct TranscriptModel {
    /// Stable model identifier.
    id: String,
    /// Model version recorded for reproducibility.
    version: String,
    /// Runtime or model variant, such as `cpu` or `cuda`.
    variant: String,
}

#[derive(Deserialize)]
struct TranscriptModelWire {
    id: String,
    version: String,
    variant: String,
}

impl<'de> Deserialize<'de> for TranscriptModel {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let wire = TranscriptModelWire::deserialize(deserializer)?;
        Self::new(wire.id, wire.version, wire.variant).map_err(serde::de::Error::custom)
    }
}

impl TranscriptModel {
    /// Stable model identifier.
    #[must_use]
    pub fn id(&self) -> &str {
        &self.id
    }

    /// Model version recorded for reproducibility.
    #[must_use]
    pub fn version(&self) -> &str {
        &self.version
    }

    /// Runtime or model variant.
    #[must_use]
    pub fn variant(&self) -> &str {
        &self.variant
    }
    /// Creates validated model metadata.
    ///
    /// # Errors
    ///
    /// Returns [`TranscriptValidationError::ModelIdentity`] when any value is
    /// empty or contains a control character.
    pub fn new(
        id: impl Into<String>,
        version: impl Into<String>,
        variant: impl Into<String>,
    ) -> Result<Self, TranscriptValidationError> {
        let model = Self {
            id: id.into(),
            version: version.into(),
            variant: variant.into(),
        };
        model.validate()?;
        Ok(model)
    }

    /// Validates the identity fields required by transcript persistence.
    ///
    /// # Errors
    ///
    /// Returns [`TranscriptValidationError::ModelIdentity`] when any value is
    /// empty or contains a control character.
    pub fn validate(&self) -> Result<(), TranscriptValidationError> {
        [&self.id, &self.version, &self.variant]
            .into_iter()
            .all(|value| !value.is_empty() && !value.chars().any(char::is_control))
            .then_some(())
            .ok_or(TranscriptValidationError::ModelIdentity)
    }
}

/// Whether a transcript segment is a replaceable interim result or final text.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum TranscriptSegmentState {
    /// A partial result that may be replaced by a later revision.
    Interim,
    /// A completed result included in materialized transcript output.
    Final,
}

impl From<bool> for TranscriptSegmentState {
    fn from(is_final: bool) -> Self {
        if is_final { Self::Final } else { Self::Interim }
    }
}

impl From<TranscriptSegmentState> for bool {
    fn from(state: TranscriptSegmentState) -> Self {
        matches!(state, TranscriptSegmentState::Final)
    }
}

/// One transcript segment, schema version 1.
///
/// Times are session monotonics in milliseconds per
/// `spec/04-storage-and-transcripts.md`. Interim revisions reuse
/// `segment_id`; they are appended, never replaced in the log.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct TranscriptSegment {
    pub(crate) schema_version: u32,
    pub(crate) segment_id: SegmentId,
    pub(crate) source: String,
    pub(crate) start_ms: u64,
    pub(crate) end_ms: u64,
    pub(crate) text: String,
    #[serde(rename = "final")]
    pub(crate) is_final: bool,
    pub(crate) model: Option<TranscriptModel>,
    /// Session-clock discontinuities overlapping this segment, in µs.
    #[serde(default)]
    pub(crate) audio_discontinuities: Vec<u64>,
}

#[derive(Deserialize)]
struct TranscriptSegmentWire {
    schema_version: u32,
    segment_id: SegmentId,
    source: String,
    start_ms: u64,
    end_ms: u64,
    text: String,
    #[serde(rename = "final")]
    is_final: bool,
    model: Option<TranscriptModel>,
    #[serde(default)]
    audio_discontinuities: Vec<u64>,
}

impl<'de> Deserialize<'de> for TranscriptSegment {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let wire = TranscriptSegmentWire::deserialize(deserializer)?;
        if wire.schema_version != TRANSCRIPT_SCHEMA_VERSION {
            return Err(serde::de::Error::custom(
                TranscriptValidationError::SchemaVersion,
            ));
        }
        Self::new(
            wire.segment_id,
            wire.source,
            wire.start_ms,
            wire.end_ms,
            wire.text,
            wire.is_final.into(),
            wire.model,
            wire.audio_discontinuities,
        )
        .map_err(serde::de::Error::custom)
    }
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
    /// Starts a validated builder for callers that need custom identity,
    /// source, model metadata, or discontinuities without an eight-argument
    /// positional constructor.
    pub fn builder(
        start_ms: u64,
        end_ms: u64,
        text: impl Into<String>,
    ) -> TranscriptSegmentBuilder {
        TranscriptSegmentBuilder {
            segment_id: SegmentId::new(),
            source: "mixed".to_owned(),
            start_ms,
            end_ms,
            text: text.into(),
            state: TranscriptSegmentState::Final,
            model: None,
            audio_discontinuities: Vec::new(),
        }
    }

    /// Schema version represented by this segment.
    #[must_use]
    pub const fn schema_version(&self) -> u32 {
        self.schema_version
    }
    /// Stable identity shared by revisions.
    #[must_use]
    pub const fn segment_id(&self) -> SegmentId {
        self.segment_id
    }
    /// Capture source label.
    #[must_use]
    pub fn source(&self) -> &str {
        &self.source
    }
    /// Inclusive segment start in session milliseconds.
    #[must_use]
    pub const fn start_ms(&self) -> u64 {
        self.start_ms
    }
    /// Segment end in session milliseconds.
    #[must_use]
    pub const fn end_ms(&self) -> u64 {
        self.end_ms
    }
    /// Recognized text exactly as persisted.
    #[must_use]
    pub fn text(&self) -> &str {
        &self.text
    }
    /// Model metadata, when ASR produced the segment.
    #[must_use]
    pub const fn model(&self) -> Option<&TranscriptModel> {
        self.model.as_ref()
    }
    /// Overlapping session discontinuities in microseconds.
    #[must_use]
    pub fn audio_discontinuities(&self) -> &[u64] {
        &self.audio_discontinuities
    }
    /// Validates the invariants required by transcript persistence.
    ///
    /// # Errors
    ///
    /// Returns a structured reason for the first invalid field group.
    pub fn validate(&self) -> Result<(), TranscriptValidationError> {
        if self.schema_version != TRANSCRIPT_SCHEMA_VERSION {
            return Err(TranscriptValidationError::SchemaVersion);
        }
        if self.start_ms > self.end_ms {
            return Err(TranscriptValidationError::Timestamps);
        }
        if self.source.is_empty() || self.source.chars().any(char::is_control) {
            return Err(TranscriptValidationError::Source);
        }
        if let Some(model) = &self.model {
            model.validate()?;
        }
        Ok(())
    }

    /// Creates a segment with an explicit stable ID and validates it.
    ///
    /// This is the lossless constructor for adapters that already assign
    /// segment IDs. For the common case where koe should create the ID and use
    /// the `mixed` source, use [`Self::interim_segment`] or
    /// [`Self::final_segment`].
    ///
    /// # Errors
    ///
    /// Returns the first semantic validation failure.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        segment_id: SegmentId,
        source: impl Into<String>,
        start_ms: u64,
        end_ms: u64,
        text: impl Into<String>,
        state: TranscriptSegmentState,
        model: Option<TranscriptModel>,
        audio_discontinuities: Vec<u64>,
    ) -> Result<Self, TranscriptValidationError> {
        let segment = Self {
            schema_version: TRANSCRIPT_SCHEMA_VERSION,
            segment_id,
            source: source.into(),
            start_ms,
            end_ms,
            text: text.into(),
            is_final: state.into(),
            model,
            audio_discontinuities,
        };
        segment.validate()?;
        Ok(segment)
    }

    /// Creates and validates an interim segment with a fresh ID.
    ///
    /// # Errors
    ///
    /// Returns the first semantic validation failure.
    pub fn interim_segment(
        start_ms: u64,
        end_ms: u64,
        text: impl Into<String>,
        model: Option<TranscriptModel>,
        audio_discontinuities: Vec<u64>,
    ) -> Result<Self, TranscriptValidationError> {
        Self::new(
            SegmentId::new(),
            "mixed",
            start_ms,
            end_ms,
            text,
            TranscriptSegmentState::Interim,
            model,
            audio_discontinuities,
        )
    }

    /// Creates and validates a final segment with the schema version stamp.
    ///
    /// # Errors
    ///
    /// Returns the first semantic validation failure.
    pub fn final_segment(
        start_ms: u64,
        end_ms: u64,
        text: impl Into<String>,
        model: Option<TranscriptModel>,
        audio_discontinuities: Vec<u64>,
    ) -> Result<Self, TranscriptValidationError> {
        Self::new(
            SegmentId::new(),
            "mixed",
            start_ms,
            end_ms,
            text,
            TranscriptSegmentState::Final,
            model,
            audio_discontinuities,
        )
    }

    /// Returns whether this is an interim or final segment revision.
    #[must_use]
    pub const fn state(&self) -> TranscriptSegmentState {
        if self.is_final {
            TranscriptSegmentState::Final
        } else {
            TranscriptSegmentState::Interim
        }
    }

    /// Returns `true` when this revision is included in finalized output.
    #[must_use]
    pub const fn is_final(&self) -> bool {
        self.is_final
    }

    /// Returns `true` when this revision may be replaced by a later result.
    #[must_use]
    pub const fn is_interim(&self) -> bool {
        !self.is_final
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
        self.revise_with_state(end_ms, text, is_final.into(), audio_discontinuities)
    }

    /// Creates and validates a revision while preserving identity and model.
    ///
    /// Prefer this to [`Self::revise`] at new call sites because the state is
    /// self-documenting rather than a boolean argument.
    ///
    /// # Errors
    ///
    /// Returns the first semantic validation failure.
    pub fn revise_with_state(
        &self,
        end_ms: u64,
        text: impl Into<String>,
        state: TranscriptSegmentState,
        audio_discontinuities: Vec<u64>,
    ) -> Result<Self, TranscriptValidationError> {
        Self::new(
            self.segment_id,
            self.source.clone(),
            self.start_ms,
            end_ms,
            text,
            state,
            self.model.clone(),
            audio_discontinuities,
        )
    }
}

/// Fluent construction for a transcript segment with explicit domain names at
/// each call site.
#[derive(Clone, Debug)]
#[must_use]
pub struct TranscriptSegmentBuilder {
    segment_id: SegmentId,
    source: String,
    start_ms: u64,
    end_ms: u64,
    text: String,
    state: TranscriptSegmentState,
    model: Option<TranscriptModel>,
    audio_discontinuities: Vec<u64>,
}

impl TranscriptSegmentBuilder {
    /// Uses a caller-assigned stable segment identity.
    pub const fn segment_id(
        mut self,
        id: SegmentId,
    ) -> Self {
        self.segment_id = id;
        self
    }
    /// Sets the source label.
    pub fn source(
        mut self,
        source: impl Into<String>,
    ) -> Self {
        self.source = source.into();
        self
    }
    /// Selects interim or final semantics.
    pub const fn state(
        mut self,
        state: TranscriptSegmentState,
    ) -> Self {
        self.state = state;
        self
    }
    /// Attaches model metadata.
    pub fn model(
        mut self,
        model: TranscriptModel,
    ) -> Self {
        self.model = Some(model);
        self
    }
    /// Attaches overlapping discontinuity timestamps.
    pub fn audio_discontinuities(
        mut self,
        values: Vec<u64>,
    ) -> Self {
        self.audio_discontinuities = values;
        self
    }
    /// Validates and constructs the segment.
    ///
    /// # Errors
    ///
    /// Returns the first semantic validation failure.
    pub fn build(self) -> Result<TranscriptSegment, TranscriptValidationError> {
        TranscriptSegment::new(
            self.segment_id,
            self.source,
            self.start_ms,
            self.end_ms,
            self.text,
            self.state,
            self.model,
            self.audio_discontinuities,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::{
        SegmentId, TranscriptModel, TranscriptSegment, TranscriptSegmentState,
        TranscriptValidationError,
    };

    #[test]
    fn constructors_validate_and_expose_state() {
        let model = TranscriptModel::new("fixture", "1", "cpu").expect("valid model");
        let interim = TranscriptSegment::interim_segment(10, 20, "hel", Some(model), Vec::new())
            .expect("valid interim");
        assert_eq!(interim.state(), TranscriptSegmentState::Interim);
        assert!(interim.is_interim());
        assert!(!interim.is_final());

        let final_segment = interim
            .revise_with_state(30, "hello", TranscriptSegmentState::Final, Vec::new())
            .expect("valid final revision");
        assert_eq!(final_segment.segment_id, interim.segment_id);
        assert_eq!(final_segment.state(), TranscriptSegmentState::Final);
        assert!(final_segment.is_final());
    }

    #[test]
    fn explicit_constructor_preserves_adapter_segment_id() {
        let id = SegmentId::new();
        let segment = TranscriptSegment::new(
            id,
            "microphone",
            0,
            1,
            "hello",
            TranscriptSegmentState::Final,
            None,
            Vec::new(),
        )
        .expect("valid segment");
        assert_eq!(segment.segment_id, id);
        assert_eq!(segment.source, "microphone");
    }

    #[test]
    fn constructors_reject_invalid_values() {
        assert_eq!(
            TranscriptModel::new("", "1", "cpu"),
            Err(TranscriptValidationError::ModelIdentity)
        );
        assert_eq!(
            TranscriptSegment::final_segment(2, 1, "reversed", None, Vec::new()),
            Err(TranscriptValidationError::Timestamps)
        );
    }

    #[test]
    fn deserialization_cannot_bypass_segment_invariants() {
        let id = SegmentId::new();
        let reversed = serde_json::json!({
            "schema_version": 1,
            "segment_id": id,
            "source": "mixed",
            "start_ms": 20,
            "end_ms": 10,
            "text": "bad",
            "final": true,
            "model": null,
            "audio_discontinuities": []
        });
        assert!(serde_json::from_value::<TranscriptSegment>(reversed).is_err());

        let unsupported = serde_json::json!({
            "schema_version": 99,
            "segment_id": id,
            "source": "mixed",
            "start_ms": 0,
            "end_ms": 10,
            "text": "bad",
            "final": true,
            "model": null
        });
        assert!(serde_json::from_value::<TranscriptSegment>(unsupported).is_err());
    }

    #[test]
    fn builder_names_optional_domain_values() {
        let id = SegmentId::new();
        let segment = TranscriptSegment::builder(0, 10, "hello")
            .segment_id(id)
            .source("microphone")
            .state(TranscriptSegmentState::Interim)
            .audio_discontinuities(vec![5])
            .build()
            .expect("valid");
        assert_eq!(segment.segment_id(), id);
        assert_eq!(segment.source(), "microphone");
        assert_eq!(segment.audio_discontinuities(), &[5]);
    }

    #[test]
    fn validation_table_covers_persistence_boundaries() {
        for source in ["", "microphone\n"] {
            assert_eq!(
                TranscriptSegment::builder(0, 1, "text")
                    .source(source)
                    .build(),
                Err(TranscriptValidationError::Source)
            );
        }
        for (id, version, variant) in [
            ("", "1", "cpu"),
            ("fixture", "", "cpu"),
            ("fixture", "1\n", "cpu"),
            ("fixture", "1", ""),
        ] {
            assert_eq!(
                TranscriptModel::new(id, version, variant),
                Err(TranscriptValidationError::ModelIdentity)
            );
        }
        assert_eq!(
            TranscriptSegment::builder(2, 1, "text").build(),
            Err(TranscriptValidationError::Timestamps)
        );
    }

    #[test]
    fn validated_model_and_segment_round_trip() {
        let model = TranscriptModel::new("fixture", "1", "cpu").expect("model");
        let segment = TranscriptSegment::builder(0, 10, "hello")
            .model(model)
            .build()
            .expect("segment");
        let encoded = serde_json::to_vec(&segment).expect("serialize");
        let decoded: TranscriptSegment = serde_json::from_slice(&encoded).expect("deserialize");
        assert_eq!(decoded, segment);

        let invalid_model = serde_json::json!({"id":"", "version":"1", "variant":"cpu"});
        assert!(serde_json::from_value::<TranscriptModel>(invalid_model).is_err());
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
