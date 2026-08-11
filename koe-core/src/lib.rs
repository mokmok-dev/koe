//! koe-core — shared pipeline, AEC, codecs, and state.

pub mod aec;
pub mod codec;
pub mod pipeline;
pub mod transcript;

pub use pipeline::{
    AudioChunk, ConsumerContext, FileWriter, PipelineConfig, PipelineError, PipelineMetrics,
    PipelineMetricsSnapshot, PipelineState, RecordingPipeline, SpeechFeeder, TranscriptionFeeder,
};
