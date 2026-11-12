//! The `transcript.txt` rendered beside the session record.

use std::fs::{self, File};
use std::io::Write;
use std::path::Path;

use crate::error::{Error, io_error};
use crate::record;
use crate::transcripts::Transcripts;

/// Name of the rendered transcript inside a meeting directory.
pub const TRANSCRIPT_FILE: &str = "transcript.txt";
/// Render being written; never read.
pub const PARTIAL_FILE: &str = "transcript.partial";

/// Rewrites `directory`'s `transcript.txt` from `transcripts`.
///
/// The text is derived from the record, so a failed write leaves the previous
/// copy in place and the next save rewrites it.
pub fn write(directory: &Path, transcripts: &Transcripts) -> Result<(), Error> {
    let partial = directory.join(PARTIAL_FILE);
    let mut file = File::create(&partial).map_err(io_error("create transcript"))?;
    file.write_all(transcripts.render().as_bytes())
        .map_err(io_error("write transcript"))?;
    file.sync_all().map_err(io_error("sync transcript"))?;
    drop(file);
    fs::rename(&partial, directory.join(TRANSCRIPT_FILE)).map_err(io_error("rename transcript"))?;
    record::sync_directory(directory);
    Ok(())
}
