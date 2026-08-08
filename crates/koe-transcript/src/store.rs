//! Append-only JSONL event log plus final materialization.

use std::{
    collections::BTreeMap,
    fs::{self, File},
    io::{BufRead, BufReader, BufWriter, Read, Write},
    path::{Path, PathBuf},
};

#[cfg(not(unix))]
use std::fs::OpenOptions;
#[cfg(unix)]
use std::os::fd::OwnedFd;
#[cfg(windows)]
use std::os::windows::fs::OpenOptionsExt;

use uuid::Uuid;

use super::types::{SegmentId, TranscriptError, TranscriptSegment};

const EVENTS_FILE: &str = "events.jsonl";
const LOCK_FILE: &str = ".events.lock";
const FINAL_JSON: &str = "final.json";
const FINAL_TEXT: &str = "final.txt";
/// Maximum encoded JSONL bytes for one transcript event (4 MiB).
pub const MAX_EVENT_RECORD_BYTES: usize = 4 * 1024 * 1024;

struct LimitedRecordWriter {
    bytes: Vec<u8>,
    limit: usize,
    exceeded: bool,
}

impl LimitedRecordWriter {
    fn new(limit: usize) -> Self {
        Self {
            bytes: Vec::with_capacity(limit.min(8 * 1024)),
            limit,
            exceeded: false,
        }
    }
}

impl Write for LimitedRecordWriter {
    fn write(
        &mut self,
        buffer: &[u8],
    ) -> std::io::Result<usize> {
        if buffer.len() > self.limit.saturating_sub(self.bytes.len()) {
            self.exceeded = true;
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "transcript record too large",
            ));
        }
        self.bytes.extend_from_slice(buffer);
        Ok(buffer.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

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
    #[cfg(unix)]
    directory_handle: OwnedFd,
    writer: Option<BufWriter<File>>,
    lock_file: Option<File>,
    /// Latest revision per segment id.
    latest: BTreeMap<SegmentId, TranscriptSegment>,
    /// First-seen append order of segment ids.
    order: Vec<SegmentId>,
    finalized: bool,
}

impl TranscriptStore {
    /// Opens a transcript directory, creating its final component if absent.
    ///
    /// Recovery durably truncates a final record lacking a newline. Complete
    /// malformed or oversized records are treated as corruption and preserved
    /// to avoid destructive automatic recovery.
    ///
    /// # Errors
    ///
    /// Returns [`TranscriptError::PathRejected`] when the directory is unsafe
    /// or `events.jsonl` is a symlink/non-file,
    /// [`TranscriptError::StoreLocked`] when another store owns the log,
    /// [`TranscriptError::CorruptLog`] for malformed/oversized complete
    /// records, and [`TranscriptError::WriteFailed`] on I/O errors.
    pub fn open(directory: impl Into<PathBuf>) -> Result<Self, TranscriptError> {
        let directory = directory.into();
        let metadata = match fs::symlink_metadata(&directory) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                fs::create_dir(&directory).map_err(|_| TranscriptError::WriteFailed)?;
                fs::symlink_metadata(&directory).map_err(|_| TranscriptError::PathRejected)?
            },
            Err(_) => return Err(TranscriptError::PathRejected),
        };
        if metadata.file_type().is_symlink() || !metadata.is_dir() {
            return Err(TranscriptError::PathRejected);
        }
        #[cfg(unix)]
        let directory_handle = open_directory_handle(&directory)?;
        let log_path = directory.join(EVENTS_FILE);
        match fs::symlink_metadata(&log_path) {
            Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_file() => {
                return Err(TranscriptError::PathRejected);
            },
            Ok(_) => {},
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {},
            Err(_) => return Err(TranscriptError::WriteFailed),
        }
        let lock_path = directory.join(LOCK_FILE);
        match fs::symlink_metadata(&lock_path) {
            Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_file() => {
                return Err(TranscriptError::PathRejected);
            },
            Ok(_) => {},
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {},
            Err(_) => return Err(TranscriptError::WriteFailed),
        }
        let mut store = Self {
            directory,
            #[cfg(unix)]
            directory_handle,
            writer: None,
            lock_file: None,
            latest: BTreeMap::new(),
            order: Vec::new(),
            finalized: false,
        };
        let lock_file = store.open_lock_file()?;
        fs2::FileExt::try_lock_exclusive(&lock_file).map_err(|error| {
            if error.kind() == std::io::ErrorKind::WouldBlock {
                TranscriptError::StoreLocked
            } else {
                TranscriptError::WriteFailed
            }
        })?;
        store.lock_file = Some(lock_file);
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
        match fs::symlink_metadata(log_path) {
            Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_file() => {
                return Err(TranscriptError::PathRejected);
            },
            Ok(_) => {},
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                self.open_writer(log_path)?;
                return Ok(());
            },
            Err(_) => return Err(TranscriptError::WriteFailed),
        }
        let mut file = self.open_log_file(false, false)?;
        if !file
            .metadata()
            .map_err(|_| TranscriptError::WriteFailed)?
            .is_file()
        {
            return Err(TranscriptError::PathRejected);
        }
        let file_len = file
            .metadata()
            .map_err(|_| TranscriptError::WriteFailed)?
            .len();
        let mut reader = BufReader::new(&mut file);
        let mut valid_len = 0_u64;
        let mut line = Vec::new();
        loop {
            line.clear();
            let mut limited = (&mut reader).take(
                u64::try_from(MAX_EVENT_RECORD_BYTES + 1)
                    .map_err(|_| TranscriptError::WriteFailed)?,
            );
            let read = limited
                .read_until(b'\n', &mut line)
                .map_err(|_| TranscriptError::WriteFailed)?;
            if read == 0 {
                break;
            }
            if line.len() > MAX_EVENT_RECORD_BYTES {
                if line.ends_with(b"\n") || scan_to_record_end(&mut reader)? {
                    return Err(TranscriptError::CorruptLog);
                }
                break;
            }
            if !line.ends_with(b"\n") {
                break;
            }
            let record = &line[..line.len() - 1];
            let segment = serde_json::from_slice::<TranscriptSegment>(record)
                .map_err(|_| TranscriptError::CorruptLog)?;
            if segment.validate().is_err() {
                return Err(TranscriptError::CorruptLog);
            }
            self.insert(segment);
            valid_len = valid_len
                .checked_add(u64::try_from(read).map_err(|_| TranscriptError::WriteFailed)?)
                .ok_or(TranscriptError::WriteFailed)?;
        }
        drop(reader);
        if valid_len != file_len {
            file.set_len(valid_len)
                .map_err(|_| TranscriptError::WriteFailed)?;
            file.sync_all().map_err(|_| TranscriptError::WriteFailed)?;
        }
        self.open_writer(log_path)
    }

    /// Appends a segment revision to the log.
    ///
    /// # Errors
    ///
    /// Returns [`TranscriptError::AlreadyFinalized`] after finalize,
    /// [`TranscriptError::InvalidSegment`] unless schema version is 1,
    /// timestamps are ordered, and source/model identity fields are nonempty
    /// and control-free, [`TranscriptError::RecordTooLarge`] above
    /// [`MAX_EVENT_RECORD_BYTES`], and [`TranscriptError::WriteFailed`] on I/O
    /// errors.
    pub fn append(
        &mut self,
        segment: TranscriptSegment,
    ) -> Result<(), TranscriptError> {
        if self.finalized {
            return Err(TranscriptError::AlreadyFinalized);
        }
        if let Err(reason) = segment.validate() {
            return Err(TranscriptError::InvalidSegment(reason));
        }
        let mut serialized = LimitedRecordWriter::new(MAX_EVENT_RECORD_BYTES - 1);
        if serde_json::to_writer(&mut serialized, &segment).is_err() {
            return Err(if serialized.exceeded {
                TranscriptError::RecordTooLarge
            } else {
                TranscriptError::WriteFailed
            });
        }
        serialized.bytes.push(b'\n');
        self.writer
            .as_mut()
            .ok_or(TranscriptError::WriteFailed)?
            .write_all(&serialized.bytes)
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
        if let Some(writer) = self.writer.as_mut() {
            writer.flush().map_err(|_| TranscriptError::WriteFailed)?;
            writer
                .get_ref()
                .sync_all()
                .map_err(|_| TranscriptError::WriteFailed)?;
        }
        drop(self.writer.take());
        let final_segments = self
            .order
            .iter()
            .filter_map(|id| self.latest.get(id))
            .filter(|segment| segment.is_final)
            .cloned()
            .collect::<Vec<_>>();
        let json_path = self.directory.join(FINAL_JSON);
        self.write_output_atomic(FINAL_JSON, &mut |writer| {
            serde_json::to_writer_pretty(&mut *writer, &final_segments)
                .map_err(|_| TranscriptError::WriteFailed)?;
            writer
                .write_all(b"\n")
                .map_err(|_| TranscriptError::WriteFailed)
        })?;
        let text_path = self.directory.join(FINAL_TEXT);
        self.write_output_atomic(FINAL_TEXT, &mut |writer| {
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

    fn write_output_atomic(
        &self,
        file_name: &str,
        write: &mut dyn FnMut(&mut BufWriter<File>) -> Result<(), TranscriptError>,
    ) -> Result<(), TranscriptError> {
        #[cfg(unix)]
        {
            use rustix::fs::{AtFlags, Mode, OFlags, fsync, openat, renameat, unlinkat};

            let temporary = format!(".{file_name}.{}.tmp", Uuid::new_v4());
            let descriptor = openat(
                &self.directory_handle,
                temporary.as_str(),
                OFlags::CLOEXEC | OFlags::NOFOLLOW | OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL,
                Mode::from_bits_truncate(0o600),
            )
            .map_err(|_| TranscriptError::WriteFailed)?;
            let mut writer = BufWriter::new(File::from(descriptor));
            let result = write(&mut writer).and_then(|()| {
                writer.flush().map_err(|_| TranscriptError::WriteFailed)?;
                writer
                    .get_ref()
                    .sync_all()
                    .map_err(|_| TranscriptError::WriteFailed)
            });
            if result.is_err() {
                let _ignored =
                    unlinkat(&self.directory_handle, temporary.as_str(), AtFlags::empty());
                return result;
            }
            if renameat(
                &self.directory_handle,
                temporary.as_str(),
                &self.directory_handle,
                file_name,
            )
            .is_err()
            {
                let _ignored =
                    unlinkat(&self.directory_handle, temporary.as_str(), AtFlags::empty());
                return Err(TranscriptError::WriteFailed);
            }
            fsync(&self.directory_handle).map_err(|_| TranscriptError::WriteFailed)
        }
        #[cfg(not(unix))]
        {
            write_temp_then_rename(&self.directory.join(file_name), write)
        }
    }

    fn sync_directory(&self) -> Result<(), TranscriptError> {
        #[cfg(unix)]
        {
            rustix::fs::fsync(&self.directory_handle).map_err(|_| TranscriptError::WriteFailed)
        }
        #[cfg(not(unix))]
        {
            Ok(())
        }
    }

    fn classify_lock_open_error(&self) -> TranscriptError {
        match fs::symlink_metadata(self.directory.join(LOCK_FILE)) {
            Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_file() => {
                TranscriptError::PathRejected
            },
            _ => TranscriptError::WriteFailed,
        }
    }

    fn open_lock_file(&self) -> Result<File, TranscriptError> {
        #[cfg(unix)]
        {
            use rustix::fs::{Mode, OFlags, openat};
            let descriptor = openat(
                &self.directory_handle,
                LOCK_FILE,
                OFlags::CLOEXEC | OFlags::NOFOLLOW | OFlags::RDWR | OFlags::CREATE,
                Mode::from_bits_truncate(0o600),
            )
            .map_err(|_| self.classify_lock_open_error())?;
            let file = File::from(descriptor);
            if !file
                .metadata()
                .map_err(|_| TranscriptError::WriteFailed)?
                .is_file()
            {
                return Err(TranscriptError::PathRejected);
            }
            Ok(file)
        }
        #[cfg(not(unix))]
        {
            let mut options = OpenOptions::new();
            options.read(true).write(true).create(true);
            set_no_follow(&mut options);
            let file = options
                .open(self.directory.join(LOCK_FILE))
                .map_err(|_| self.classify_lock_open_error())?;
            if !file
                .metadata()
                .map_err(|_| TranscriptError::WriteFailed)?
                .is_file()
            {
                return Err(TranscriptError::PathRejected);
            }
            Ok(file)
        }
    }

    fn open_log_file(
        &self,
        append: bool,
        create: bool,
    ) -> Result<File, TranscriptError> {
        #[cfg(unix)]
        {
            use rustix::fs::{Mode, OFlags, openat};

            let mut flags = OFlags::CLOEXEC | OFlags::NOFOLLOW | OFlags::RDWR;
            if append {
                flags |= OFlags::APPEND;
            }
            if create {
                flags |= OFlags::CREATE;
            }
            let descriptor = openat(
                &self.directory_handle,
                EVENTS_FILE,
                flags,
                Mode::from_bits_truncate(0o600),
            )
            .map_err(|_| TranscriptError::WriteFailed)?;
            Ok(File::from(descriptor))
        }
        #[cfg(not(unix))]
        {
            let mut options = OpenOptions::new();
            options
                .read(!append)
                .write(!append)
                .append(append)
                .create(create);
            set_no_follow(&mut options);
            options
                .open(self.directory.join(EVENTS_FILE))
                .map_err(|_| TranscriptError::WriteFailed)
        }
    }

    fn open_writer(
        &mut self,
        log_path: &Path,
    ) -> Result<(), TranscriptError> {
        let created = match fs::symlink_metadata(log_path) {
            Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_file() => {
                return Err(TranscriptError::PathRejected);
            },
            Ok(_) => false,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => true,
            Err(_) => return Err(TranscriptError::WriteFailed),
        };
        let file = self.open_log_file(true, true)?;
        self.writer = Some(BufWriter::new(file));
        if created {
            self.sync_directory()?;
        }
        Ok(())
    }
}

fn scan_to_record_end(reader: &mut impl BufRead) -> Result<bool, TranscriptError> {
    loop {
        let buffer = reader
            .fill_buf()
            .map_err(|_| TranscriptError::WriteFailed)?;
        if buffer.is_empty() {
            return Ok(false);
        }
        if let Some(index) = buffer.iter().position(|byte| *byte == b'\n') {
            reader.consume(index + 1);
            return Ok(true);
        }
        let consumed = buffer.len();
        reader.consume(consumed);
    }
}

#[cfg(unix)]
fn open_directory_handle(path: &Path) -> Result<OwnedFd, TranscriptError> {
    use rustix::fs::{Mode, OFlags, open};

    open(
        path,
        OFlags::CLOEXEC | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::RDONLY,
        Mode::empty(),
    )
    .map_err(|_| TranscriptError::PathRejected)
}

#[cfg(not(unix))]
fn set_no_follow(options: &mut OpenOptions) {
    #[cfg(windows)]
    options.custom_flags(0x0020_0000); // FILE_FLAG_OPEN_REPARSE_POINT
}

#[cfg(not(unix))]
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
    replace_file(&temporary, destination)
}

#[cfg(not(any(unix, windows)))]
fn replace_file(
    source: &Path,
    destination: &Path,
) -> Result<(), TranscriptError> {
    fs::rename(source, destination).map_err(|_| TranscriptError::WriteFailed)
}

#[cfg(windows)]
fn replace_file(
    source: &Path,
    destination: &Path,
) -> Result<(), TranscriptError> {
    use atomicwrites::{AllowOverwrite, AtomicFile};
    let atomic = AtomicFile::new(destination, AllowOverwrite);
    atomic
        .write(|output| {
            let mut input = File::open(source)?;
            std::io::copy(&mut input, output)?;
            Ok::<(), std::io::Error>(())
        })
        .map_err(|_| TranscriptError::WriteFailed)?;
    fs::remove_file(source).map_err(|_| TranscriptError::WriteFailed)
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

    fn final_segment(
        start_ms: u64,
        end_ms: u64,
        text: String,
        model: Option<TranscriptModel>,
        audio_discontinuities: Vec<u64>,
    ) -> TranscriptSegment {
        TranscriptSegment::final_segment(start_ms, end_ms, text, model, audio_discontinuities)
            .expect("valid segment")
    }

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
            .append(final_segment(
                0,
                1000,
                "hello".to_owned(),
                Some(model()),
                Vec::new(),
            ))
            .expect("append");
        store
            .append(final_segment(
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
        let interim = final_segment(0, 500, "partial".to_owned(), Some(model()), Vec::new());
        let id = interim.segment_id;
        store.append(interim.clone()).expect("interim");
        store
            .append(
                interim
                    .revise(1000, "final text".to_owned(), true, Vec::new())
                    .expect("valid revision"),
            )
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
        let complete = serde_json::to_string(&final_segment(
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
            .append(final_segment(
                5000,
                6000,
                "after".to_owned(),
                Some(model()),
                Vec::new(),
            ))
            .expect("append");
        store.checkpoint().expect("checkpoint");
        drop(store);

        let mut store = open(&directory);
        assert_eq!(store.segments().len(), 2, "recovered log must remain valid");
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
            .append(final_segment(0, 1, "x".to_owned(), None, Vec::new()))
            .expect("append");
        store.finalize().expect("finalize");
        assert!(store.finalize().is_err());
        assert!(
            store
                .append(final_segment(1, 2, "y".to_owned(), None, Vec::new()))
                .is_err()
        );
    }
}
