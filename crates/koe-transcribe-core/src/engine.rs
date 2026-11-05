//! The seam between the facade and a platform backend.

use alloc::boxed::Box;

use crate::error::Error;

/// Where the speech model comes from.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum Model {
    /// A GGUF model file on disk.
    Gguf(Box<str>),
    /// The speech model the operating system provides.
    SystemSpeech,
}

impl Model {
    /// A GGUF model file on disk.
    #[must_use]
    pub fn gguf(path: impl Into<Box<str>>) -> Self {
        Self::Gguf(path.into())
    }

    /// The speech model the operating system provides.
    #[must_use]
    pub const fn system_speech() -> Self {
        Self::SystemSpeech
    }
}

/// Per-session configuration.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Options {
    language: Option<Box<str>>,
}

impl Options {
    /// Defaults: let the engine pick the language.
    #[must_use]
    pub const fn new() -> Self {
        Self { language: None }
    }

    /// Requests a language by BCP 47 tag, for example `ja` or `pt-BR`.
    #[must_use]
    pub fn language(mut self, tag: impl Into<Box<str>>) -> Self {
        self.language = Some(tag.into());
        self
    }

    /// The requested language tag, if any.
    #[must_use]
    pub fn language_tag(&self) -> Option<&str> {
        self.language.as_deref()
    }
}

/// A configured engine that opens sessions.
pub trait Engine: Send + Sync {
    /// Opens a session that accepts 16 kHz mono `f32` samples.
    ///
    /// # Errors
    ///
    /// Returns [`Error::ModelMissing`] when the engine has no assets for the
    /// requested language, [`Error::OptionsInvalid`] when an option is outside
    /// its range, and the backend's error when a session cannot start.
    fn new_session(&self, options: &Options) -> Result<Box<dyn EngineSession>, Error>;
}

/// One recognition pass over a stream of 16 kHz mono samples.
pub trait EngineSession: Send {
    /// Appends 16 kHz mono samples.
    ///
    /// # Errors
    ///
    /// Returns the backend's error when inference fails.
    fn feed(&mut self, pcm: &[f32]) -> Result<(), Error>;

    /// Takes the next recognition result, if one is ready.
    ///
    /// # Errors
    ///
    /// Returns the backend's error when the result stream reports a failure.
    fn poll(&mut self) -> Result<Option<EngineEvent>, Error>;

    /// Ends the stream and flushes any text the engine still holds.
    ///
    /// # Errors
    ///
    /// Returns the backend's error when the flush fails.
    fn finish(&mut self) -> Result<(), Error>;
}

/// A recognition result as a backend reports it.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum EngineEvent {
    /// Text the engine will not revise.
    Committed {
        /// Committed text.
        text: Box<str>,
        /// Offset of the first sample the text covers, in milliseconds.
        start_ms: u64,
        /// Offset one past the last sample the text covers, in milliseconds.
        end_ms: u64,
    },
    /// A provisional tail the engine may replace or withdraw.
    Volatile {
        /// Current best guess for the uncommitted tail.
        text: Box<str>,
    },
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;

    #[test]
    fn model_constructors_match_their_variants() {
        assert_eq!(
            Model::gguf("/models/tiny.gguf"),
            Model::Gguf("/models/tiny.gguf".into())
        );
        assert_eq!(Model::system_speech(), Model::SystemSpeech);
    }

    #[test]
    fn options_default_to_no_language() {
        let options = Options::new();
        assert_eq!(options, Options::default());
        assert_eq!(options.language_tag(), None);
    }

    #[test]
    fn a_requested_language_is_kept() {
        let options = Options::new().language("ja");
        assert_eq!(options.language_tag(), Some("ja"));
        let replaced = options.language(String::from("pt-BR"));
        assert_eq!(replaced.language_tag(), Some("pt-BR"));
    }
}
