//! Fallback used when no backend can serve the request.

use koe_transcribe_core::{Engine, Error, Model};

pub fn engine(_model: &Model) -> Result<Box<dyn Engine>, Error> {
    Err(Error::Unsupported)
}
