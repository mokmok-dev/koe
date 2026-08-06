//! Append-only JSONL event log plus final materialization.

use std::{
    collections::BTreeMap,
    fs::{self, File, OpenOptions},
    io::{BufWriter, Write},
    path::{Path, PathBuf},
};

use uuid::Uuid;

use super::types::{SegmentId, TranscriptError, TranscriptSegment};

const EVENTS_FILE: &str = "events.jsonl";
const FINAL_JSON: &str = "final.json";
const FINAL_TEXT: &str = "final.txt";

/// Materialized view written by [`TranscriptStore::finalize`].
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct TranscriptReport {
    pub json_path: PathBuf,
    pub text_path: PathBuf,
    pub segment_count: usize,
}

/// Append-only transcript log for one session directory.
pub struct TranscriptStore {
    directory: PathBuf,
    writer: Option<BufWriter<File>>,
    /// Latest revision per segment id.
    latest: BTreeMap<SegmentId, TranscriptSegment>,
    /// First-seen append order of segment ids.
    order: Vec<SegmentId>,
    finalized: bool,
}

impl TranscriptStore {
    /// Opens a transcript directory, creating it if absent.
    ///
    /// # Errors
    ///
    /// Returns [`TranscriptError::PathRejected`] when the directory is a
    /// symlink and [`TranscriptError::WriteFailed`] on I/O errors.
    pub fn open(directory: impl Into<PathBuf>) -> Result<Self, TranscriptError> {
        let directory = directory.into();
        let metadata =
            fs::symlink_metadata(&directory).map_err(|_| TranscriptError::PathRejected)?;
        if metadata.file_type().is_symlink() || !metadata.is_dir() {
            return Err(TranscriptError::PathRejected);
        }
        let log_path = directory.join(EVENTS_FILE);
        let mut store = Self {
            directory,
            writer: None,
            latest: BTreeMap::new(),
            order: Vec::new(),
            finalized: false,
        };
        store.recover_log(&log_path)?;
        Ok(store)
    }

    /// Opens an existing log, keeping only complete records. A torn final
    /// line (no trailing newline) is ignored; a bad middle record is fatal.
    ///
    /// # Errors
    ///
    /// Returns [`TranscriptError::CorruptLog`] for an invalid complete line
    /// and [`TranscriptError::WriteFailed`] on I/O errors.
    fn recover_log(
        &mut self,
        log_path: &Path,
    ) -> Result<(), TranscriptError> {
        let content = match fs::read(log_path) {
            Ok(content) => content,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                self.open_writer(log_path)?;
                return Ok(());
            },
            Err(_) => return Err(TranscriptError::WriteFailed),
        };
        // `split_inclusive` keeps the newline on every complete line. A final
        // line without a trailing newline is a torn write from a crash and is
        // ignored; a complete line that fails to parse is corruption.
        let mut lines = content.split_inclusive(|byte| *byte == b'\n');
        let mut pending = lines.next();
        while let Some(line) = pending.take() {
            pending = lines.next();
            if !line.ends_with(b"\n") {
                continue;
            }
            let record = &line[..line.len() - 1];
            let segment = serde_json::from_slice::<TranscriptSegment>(record)
                .map_err(|_| TranscriptError::CorruptLog)?;
            self.insert(segment);
        }
        self.open_writer(log_path)
    }

    /// Appends a segment revision to the log.
    ///
    /// # Errors
    ///
    /// Returns [`TranscriptError::AlreadyFinalized`] after finalize and
    /// [`TranscriptError::WriteFailed`] on I/O errors.
    pub fn append(
        &mut self,
        segment: TranscriptSegment,
    ) -> Result<(), TranscriptError> {
        if self.finalized {
            return Err(TranscriptError::AlreadyFinalized);
        }
        let writer = self.writer.as_mut().ok_or(TranscriptError::WriteFailed)?;
        serde_json::to_writer(&mut *writer, &segment).map_err(|_| TranscriptError::WriteFailed)?;
        writer
            .write_all(b"\n")
            .map_err(|_| TranscriptError::WriteFailed)?;
        self.insert(segment);
        Ok(())
    }

    fn insert(
        &mut self,
        segment: TranscriptSegment,
    ) {
        if !self.latest.contains_key(&segment.segment_id) {
            self.order.push(segment.segment_id);
        }
        self.latest.insert(segment.segment_id, segment);
    }

    /// Flushes (without fsync) pending event lines.
    ///
    /// # Errors
    ///
    /// Returns [`TranscriptError::WriteFailed`] on I/O errors.
    pub fn flush(&mut self) -> Result<(), TranscriptError> {
        self.writer
            .as_mut()
            .ok_or(TranscriptError::WriteFailed)?
            .flush()
            .map_err(|_| TranscriptError::WriteFailed)
    }

    /// Flushes and fsyncs the event log (checkpoint).
    ///
    /// # Errors
    ///
    /// Returns [`TranscriptError::WriteFailed`] on I/O errors.
    pub fn checkpoint(&mut self) -> Result<(), TranscriptError> {
        self.flush()?;
        self.writer
            .as_mut()
            .ok_or(TranscriptError::WriteFailed)?
            .get_ref()
            .sync_all()
            .map_err(|_| TranscriptError::WriteFailed)
    }

    /// Materializes `final.json` and `final.txt` atomically.
    ///
    /// # Errors
    ///
    /// Returns [`TranscriptError::WriteFailed`] on I/O errors.
    pub fn finalize(&mut self) -> Result<TranscriptReport, TranscriptError> {
        if self.finalized {
            return Err(TranscriptError::AlreadyFinalized);
        }
        if let Some(writer) = self.writer.take() {
            let mut writer = writer;
            writer.flush().map_err(|_| TranscriptError::WriteFailed)?;
            writer
                .get_ref()
                .sync_all()
                .map_err(|_| TranscriptError::WriteFailed)?;
        }
        let final_segments = self
            .order
            .iter()
            .filter_map(|id| self.latest.get(id))
            .filter(|segment| segment.is_final)
            .cloned()
            .collect::<Vec<_>>();
        let json_path = self.directory.join(FINAL_JSON);
        write_temp_then_rename(&json_path, &mut |writer| {
            serde_json::to_writer_pretty(&mut *writer, &final_segments)
                .map_err(|_| TranscriptError::WriteFailed)?;
            writer
                .write_all(b"\n")
                .map_err(|_| TranscriptError::WriteFailed)
        })?;
        let text_path = self.directory.join(FINAL_TEXT);
        write_temp_then_rename(&text_path, &mut |writer| {
            for segment in &final_segments {
                writer
                    .write_all(segment.text.as_bytes())
                    .map_err(|_| TranscriptError::WriteFailed)?;
                writer
                    .write_all(b"\n")
                    .map_err(|_| TranscriptError::WriteFailed)?;
            }
            Ok(())
        })?;
        self.finalized = true;
        Ok(TranscriptReport {
            json_path,
            text_path,
            segment_count: final_segments.len(),
        })
    }

    /// Latest revision per segment id, in first-seen order.
    #[must_use]
    pub fn segments(&self) -> Vec<TranscriptSegment> {
        self.order
            .iter()
            .filter_map(|id| self.latest.get(id).cloned())
            .collect()
    }

    /// Segment count.
    #[must_use]
    pub fn len(&self) -> usize {
        self.latest.len()
    }

    /// Whether no segment has been recorded.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.latest.is_empty()
    }

    fn open_writer(
        &mut self,
        log_path: &Path,
    ) -> Result<(), TranscriptError> {
        let mut options = OpenOptions::new();
        options.append(true).create(true);
        let file = options
            .open(log_path)
            .map_err(|_| TranscriptError::WriteFailed)?;
        self.writer = Some(BufWriter::new(file));
        Ok(())
    }
}

fn write_temp_then_rename(
    destination: &Path,
    write: &mut dyn FnMut(&mut BufWriter<File>) -> Result<(), TranscriptError>,
) -> Result<(), TranscriptError> {
    let parent = destination.parent().ok_or(TranscriptError::PathRejected)?;
    let file_name = destination
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or(TranscriptError::PathRejected)?;
    let temporary = parent.join(format!(".{}.{}.tmp", file_name, Uuid::new_v4()));
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    let file = options
        .open(&temporary)
        .map_err(|_| TranscriptError::WriteFailed)?;
    let mut writer = BufWriter::new(file);
    let result = write(&mut writer);
    writer.flush().map_err(|_| TranscriptError::WriteFailed)?;
    if let Err(error) = writer.get_ref().sync_all() {
        drop(writer);
        let _ignored = fs::remove_file(&temporary);
        return Err(error.into());
    }
    drop(writer);
    if result.is_err() {
        let _ignored = fs::remove_file(&temporary);
        return result;
    }
    fs::rename(&temporary, destination).map_err(|_| TranscriptError::WriteFailed)
}

impl From<std::io::Error> for TranscriptError {
    fn from(_: std::io::Error) -> Self {
        Self::WriteFailed
    }
}

#[cfg(test)]
mod tests {
    use tempfile::TempDir;

    use super::TranscriptStore;
    use crate::{TranscriptModel, TranscriptSegment};

    fn model() -> TranscriptModel {
        TranscriptModel {
            id: "fixture-model".to_owned(),
            version: "1.0".to_owned(),
            variant: "cpu".to_owned(),
        }
    }

    fn open(directory: &std::path::Path) -> TranscriptStore {
        TranscriptStore::open(directory).expect("open")
    }

    #[test]
    fn append_then_finalize_materializes_json_and_text() {
        let root = TempDir::new().expect("temp");
        let directory = root.path().join("transcript");
        std::fs::create_dir(&directory).expect("create");
        let mut store = open(&directory);
        store
            .append(TranscriptSegment::final_segment(
                0,
                1000,
                "hello".to_owned(),
                Some(model()),
                Vec::new(),
            ))
            .expect("append");
        store
            .append(TranscriptSegment::final_segment(
                1000,
                2000,
                "world".to_owned(),
                Some(model()),
                vec![1500],
            ))
            .expect("append");
        let report = store.finalize().expect("finalize");
        assert_eq!(report.segment_count, 2);
        assert!(report.json_path.exists());
        assert!(report.text_path.exists());
        let text = std::fs::read_to_string(report.text_path).expect("text");
        assert_eq!(text, "hello\nworld\n");
        let json: Vec<TranscriptSegment> =
            serde_json::from_slice(&std::fs::read(report.json_path).expect("json"))
                .expect("json parse");
        assert_eq!(json.len(), 2);
        assert!(json[1].audio_discontinuities.contains(&1500));
    }

    #[test]
    fn revisions_keep_the_latest_per_segment_in_the_view() {
        let root = TempDir::new().expect("temp");
        let directory = root.path().join("transcript");
        std::fs::create_dir(&directory).expect("create");
        let mut store = open(&directory);
        let interim = TranscriptSegment::final_segment(
            0,
            500,
            "partial".to_owned(),
            Some(model()),
            Vec::new(),
        );
        let id = interim.segment_id;
        store.append(interim.clone()).expect("interim");
        store
            .append(interim.revise(1000, "final text".to_owned(), true, Vec::new()))
            .expect("revision");
        store.checkpoint().expect("checkpoint");
        let report = store.finalize().expect("finalize");
        assert_eq!(report.segment_count, 1);
        let json: Vec<TranscriptSegment> =
            serde_json::from_slice(&std::fs::read(report.json_path).expect("json"))
                .expect("json parse");
        assert_eq!(json.len(), 1);
        assert_eq!(json[0].segment_id, id);
        assert_eq!(json[0].text, "final text");
        assert_eq!(json[0].end_ms, 1000);
    }

    #[test]
    fn recovery_preserves_complete_records_and_ignores_torn_tail() {
        let root = TempDir::new().expect("temp");
        let directory = root.path().join("transcript");
        std::fs::create_dir(&directory).expect("create");
        let log = directory.join("events.jsonl");
        let complete = serde_json::to_string(&TranscriptSegment::final_segment(
            0,
            1000,
            "kept".to_owned(),
            Some(model()),
            Vec::new(),
        ))
        .expect("json");
        std::fs::write(
            &log,
            format!("{complete}\n{{\"schema_version\":1,\"segment_id\":\"torn"),
        )
        .expect("write");
        let mut store = open(&directory);
        assert_eq!(store.segments().len(), 1, "torn tail must be ignored");
        assert_eq!(store.segments()[0].text, "kept");
        store
            .append(TranscriptSegment::final_segment(
                5000,
                6000,
                "after".to_owned(),
                Some(model()),
                Vec::new(),
            ))
            .expect("append");
        store.checkpoint().expect("checkpoint");
        let report = store.finalize().expect("finalize");
        let json: Vec<TranscriptSegment> =
            serde_json::from_slice(&std::fs::read(report.json_path).expect("json"))
                .expect("json parse");
        assert_eq!(json.len(), 2);
    }

    #[test]
    fn middle_line_corruption_is_fatal() {
        let root = TempDir::new().expect("temp");
        let directory = root.path().join("transcript");
        std::fs::create_dir(&directory).expect("create");
        let log = directory.join("events.jsonl");
        std::fs::write(&log, "{\"bad\"}\n{\"also\":\"bad\"}\n").expect("write");
        assert!(TranscriptStore::open(&directory).is_err());
    }

    #[test]
    fn finalize_and_append_reject_after_finalize() {
        let root = TempDir::new().expect("temp");
        let directory = root.path().join("transcript");
        std::fs::create_dir(&directory).expect("create");
        let mut store = open(&directory);
        store
            .append(TranscriptSegment::final_segment(
                0,
                1,
                "x".to_owned(),
                None,
                Vec::new(),
            ))
            .expect("append");
        store.finalize().expect("finalize");
        assert!(store.finalize().is_err());
        assert!(
            store
                .append(TranscriptSegment::final_segment(
                    1,
                    2,
                    "y".to_owned(),
                    None,
                    Vec::new()
                ))
                .is_err()
        );
    }
}
