//! Byte sinks for the container writers.

use crate::error::Error;

/// Where a container writer sends the bytes it produces.
pub trait Sink {
    /// Writes every byte of `bytes`.
    ///
    /// # Errors
    ///
    /// Returns the sink's own error when the write fails.
    fn write_all(&mut self, bytes: &[u8]) -> Result<(), Error>;
}

/// A sink that can also correct bytes it already accepted.
pub trait PatchSink: Sink {
    /// Overwrites the bytes starting at `offset` with `bytes`.
    ///
    /// `offset..offset + bytes.len()` must lie inside the bytes already
    /// written. The append position stays at the end, so the next `write_all`
    /// appends.
    ///
    /// # Errors
    ///
    /// Returns the sink's own error when the write fails.
    fn write_at(&mut self, offset: usize, bytes: &[u8]) -> Result<(), Error>;
}
