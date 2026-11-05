//! `SpeechAnalyzer` backend for macOS.

#[cfg(any(target_os = "macos", test))]
mod pipeline;

#[cfg(target_os = "macos")]
mod ffi;

use koe_transcribe_core::{Engine, Error, Model};

/// Prepares the system speech engine.
///
/// # Errors
///
/// Returns [`Error::Unavailable`] when the backend cannot transcribe with the
/// requested model, [`Error::Unsupported`] on targets without
/// `SpeechAnalyzer`, and the platform's error when a session cannot start.
pub fn engine(model: &Model) -> Result<Box<dyn Engine>, Error> {
    match model {
        Model::SystemSpeech => {
            #[cfg(target_os = "macos")]
            {
                Ok(Box::new(pipeline::Engine::new(ffi::SpeechShim)))
            }
            #[cfg(not(target_os = "macos"))]
            {
                Err(Error::Unsupported)
            }
        }
        Model::Gguf(_) => Err(Error::Unavailable {
            feature: "gguf transcription",
        }),
        // Later variants have no macOS system-speech mapping to fall into.
        _ => Err(Error::Unavailable {
            feature: "the requested model kind",
        }),
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;

    #[test]
    fn a_gguf_model_is_unavailable_on_the_macos_backend() {
        assert!(matches!(
            engine(&Model::gguf("/models/tiny.gguf")),
            Err(Error::Unavailable {
                feature: "gguf transcription",
            })
        ));
    }

    #[cfg(not(target_os = "macos"))]
    #[test]
    fn system_speech_is_unsupported_off_macos() {
        assert!(matches!(
            engine(&Model::system_speech()),
            Err(Error::Unsupported)
        ));
    }
}
