//! The transcript file written while recording.

use std::fs::File;
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};

use koe_capture::Source;
use koe_transcribe::Transcript;

use crate::error::AppError;

/// Appends committed segments to one transcript file.
#[derive(Debug)]
pub struct TranscriptWriter {
    file: BufWriter<File>,
    path: PathBuf,
    timestamps: bool,
    written: [usize; 2],
}

impl TranscriptWriter {
    /// Creates or truncates `path`.
    ///
    /// # Errors
    ///
    /// Returns [`AppError::Io`] when the file cannot be created.
    pub fn create(path: &Path, timestamps: bool) -> Result<Self, AppError> {
        let file = File::create(path).map_err(|source| AppError::Io {
            operation: "create transcript",
            path: path.to_path_buf(),
            source,
        })?;
        Ok(Self {
            file: BufWriter::new(file),
            path: path.to_path_buf(),
            timestamps,
            written: [0, 0],
        })
    }

    /// Writes one committed segment.
    ///
    /// # Errors
    ///
    /// Returns [`AppError::Io`] when the line cannot be written.
    pub fn write_committed(
        &mut self,
        source: Source,
        start_ms: u64,
        end_ms: u64,
        text: &str,
    ) -> Result<(), AppError> {
        let result = if self.timestamps {
            writeln!(
                self.file,
                "[{start_ms}..{end_ms}] {}: {text}",
                source.as_str()
            )
        } else {
            writeln!(self.file, "{}: {text}", source.as_str())
        };
        result.map_err(|source_error| AppError::Io {
            operation: "write transcript",
            path: self.path.clone(),
            source: source_error,
        })?;
        self.written[source_index(source)] += 1;
        Ok(())
    }

    /// Writes the segments the final transcript holds beyond the committed
    /// events already written.
    ///
    /// # Errors
    ///
    /// Returns [`AppError::Io`] when a line cannot be written.
    pub fn finish_source(
        &mut self,
        source: Source,
        transcript: &Transcript,
    ) -> Result<(), AppError> {
        let index = source_index(source);
        for segment in transcript.segments().iter().skip(self.written[index]) {
            self.write_committed(source, segment.start_ms(), segment.end_ms(), segment.text())?;
        }
        Ok(())
    }

    /// Flushes the file.
    ///
    /// # Errors
    ///
    /// Returns [`AppError::Io`] when the last bytes cannot be written.
    pub fn finish(mut self) -> Result<(), AppError> {
        self.file.flush().map_err(|source| AppError::Io {
            operation: "flush transcript",
            path: self.path,
            source,
        })
    }
}

const fn source_index(source: Source) -> usize {
    match source {
        Source::Microphone => 0,
        Source::SystemAudio => 1,
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use koe_transcribe::Segment;

    use super::*;

    #[test]
    fn committed_segments_are_lines_with_their_source() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("take.txt");
        let mut writer = TranscriptWriter::create(&path, false).unwrap();
        writer
            .write_committed(Source::Microphone, 0, 100, "hello")
            .unwrap();
        writer
            .write_committed(Source::SystemAudio, 100, 200, "world")
            .unwrap();
        writer.finish().unwrap();
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "microphone: hello\nsystem audio: world\n"
        );
    }

    #[test]
    fn timestamps_prefix_each_line() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("take.txt");
        let mut writer = TranscriptWriter::create(&path, true).unwrap();
        writer
            .write_committed(Source::Microphone, 0, 100, "hello")
            .unwrap();
        writer
            .write_committed(Source::SystemAudio, 100, 200, "world")
            .unwrap();
        writer.finish().unwrap();
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "[0..100] microphone: hello\n[100..200] system audio: world\n"
        );
    }

    #[test]
    fn the_final_transcript_adds_only_the_unwritten_segments() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("take.txt");
        let mut writer = TranscriptWriter::create(&path, false).unwrap();
        writer
            .write_committed(Source::Microphone, 0, 100, "hello")
            .unwrap();
        let transcript = Transcript::new(
            vec![
                Segment::new(0, 100, "hello"),
                Segment::new(100, 200, "world"),
            ],
            0,
        );
        writer
            .finish_source(Source::Microphone, &transcript)
            .unwrap();
        writer.finish().unwrap();
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "microphone: hello\nmicrophone: world\n"
        );
    }

    #[test]
    fn a_final_transcript_without_new_segments_adds_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("take.txt");
        let mut writer = TranscriptWriter::create(&path, false).unwrap();
        writer
            .write_committed(Source::Microphone, 0, 100, "hello")
            .unwrap();
        let transcript = Transcript::new(vec![Segment::new(0, 100, "hello")], 0);
        writer
            .finish_source(Source::Microphone, &transcript)
            .unwrap();
        writer.finish().unwrap();
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "microphone: hello\n"
        );
    }

    #[test]
    fn a_missing_parent_directory_is_an_io_error() {
        let path = Path::new("/nonexistent/koe/take.txt");
        let error = TranscriptWriter::create(path, false).unwrap_err();
        match error {
            AppError::Io {
                operation,
                path: error_path,
                ..
            } => {
                assert_eq!(operation, "create transcript");
                assert_eq!(error_path, path.to_path_buf());
            }
            other => panic!("expected an io error, got {other:?}"),
        }
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn a_flush_failure_is_an_io_error() {
        let mut writer = TranscriptWriter::create(Path::new("/dev/full"), false).unwrap();
        writer
            .write_committed(Source::Microphone, 0, 100, "hello")
            .unwrap();
        let error = writer.finish().unwrap_err();
        assert!(matches!(
            error,
            AppError::Io {
                operation: "flush transcript",
                ..
            }
        ));
    }

    #[test]
    fn source_indexes_are_distinct() {
        assert_eq!(source_index(Source::Microphone), 0);
        assert_eq!(source_index(Source::SystemAudio), 1);
    }
}
