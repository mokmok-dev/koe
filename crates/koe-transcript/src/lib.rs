//! Transcript segments, append-only JSONL store, materialization and export.
//!
//! Milestone 3 (`spec/04-storage-and-transcripts.md`) materializes
//! `events.jsonl` into `final.json` and `final.txt`. Interim results keep the
//! same `segment_id` as revisions; the materialized view contains the latest
//! revision of every final segment.

mod store;
mod timeline;
mod types;

pub use store::{MAX_EVENT_RECORD_BYTES, TranscriptReport, TranscriptStore};
pub use timeline::{TimelineSnapshot, format_clock, format_plain_text};
pub use types::{
    SegmentId, TRANSCRIPT_SCHEMA_VERSION, TranscriptError, TranscriptModel, TranscriptSegment,
    TranscriptSegmentBuilder, TranscriptSegmentState, TranscriptValidationError,
};

/// Export helpers for user-facing artifacts.
pub mod export {
    use std::{
        fs::{self, OpenOptions},
        io::{BufWriter, Write},
        path::Path,
    };

    use super::types::TranscriptError;

    /// Writes the materialized JSON array. Fails when `destination` exists.
    ///
    /// # Errors
    ///
    /// Returns [`TranscriptError::PathRejected`] when the destination exists
    /// or is a symlink and [`TranscriptError::WriteFailed`] on I/O errors.
    pub fn export_json(
        destination: &Path,
        segments: &[super::types::TranscriptSegment],
    ) -> Result<(), TranscriptError> {
        write_new(destination, &mut |writer| {
            serde_json::to_writer_pretty(&mut *writer, segments)
                .map_err(|_| TranscriptError::WriteFailed)?;
            writer
                .write_all(b"\n")
                .map_err(|_| TranscriptError::WriteFailed)
        })
    }

    /// Writes one segment text per line. Fails when `destination` exists.
    ///
    /// # Errors
    ///
    /// Returns [`TranscriptError::PathRejected`] when the destination exists
    /// or is a symlink and [`TranscriptError::WriteFailed`] on I/O errors.
    pub fn export_text(
        destination: &Path,
        segments: &[super::types::TranscriptSegment],
    ) -> Result<(), TranscriptError> {
        write_new(destination, &mut |writer| {
            for segment in segments {
                writer
                    .write_all(segment.text.as_bytes())
                    .map_err(|_| TranscriptError::WriteFailed)?;
                writer
                    .write_all(b"\n")
                    .map_err(|_| TranscriptError::WriteFailed)?;
            }
            Ok(())
        })
    }

    fn write_new(
        destination: &Path,
        write: &mut dyn FnMut(&mut BufWriter<fs::File>) -> Result<(), TranscriptError>,
    ) -> Result<(), TranscriptError> {
        if fs::symlink_metadata(destination).is_ok() {
            return Err(TranscriptError::PathRejected);
        }
        let parent = destination.parent().ok_or(TranscriptError::PathRejected)?;
        let file_name = destination
            .file_name()
            .and_then(|name| name.to_str())
            .ok_or(TranscriptError::PathRejected)?;
        let temporary = parent.join(format!(".{file_name}.tmp"));
        match OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temporary)
        {
            Ok(file) => {
                let mut writer = BufWriter::new(file);
                let result = write(&mut writer);
                writer.flush().map_err(|_| TranscriptError::WriteFailed)?;
                drop(writer);
                if result.is_ok() {
                    fs::rename(&temporary, destination)
                        .map_err(|_| TranscriptError::WriteFailed)?;
                } else {
                    let _ignored = fs::remove_file(&temporary);
                }
                result
            },
            Err(_) => Err(TranscriptError::PathRejected),
        }
    }
}
