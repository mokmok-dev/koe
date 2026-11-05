//! `transcribe.cpp` backend for Linux and Windows.
//!
//! The crate loads a GGUF model once, then opens sessions that accept 16 kHz
//! mono samples. Models with a streaming family run incrementally; the rest,
//! Whisper included, buffer the audio and recognize it in one call at the end.
//! Only the FFI module mentions `transcribe-cpp`; the text mapping sits beside
//! it and is tested without the native library.

mod ffi;
mod pipeline;

use koe_transcribe_core::{Engine, Error, Model};

/// Prepares the portable engine for `model`.
///
/// # Errors
///
/// Returns [`Error::Unavailable`] for [`Model::SystemSpeech`],
/// [`Error::ModelNotFound`] when the file does not exist, and
/// [`Error::ModelInvalid`] when the file exists but cannot be loaded.
pub fn engine(model: &Model) -> Result<Box<dyn Engine>, Error> {
    match model {
        Model::Gguf(path) => Ok(Box::new(ffi::load(path)?)),
        Model::SystemSpeech => Err(Error::Unavailable {
            feature: "system speech",
        }),
        _ => Err(Error::Unsupported),
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;

    #[test]
    fn system_speech_is_unavailable() {
        assert_eq!(
            engine(&Model::system_speech()).err(),
            Some(Error::Unavailable {
                feature: "system speech"
            })
        );
    }
}
