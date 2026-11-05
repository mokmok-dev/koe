//! Model and session option construction shared by the commands.

use std::path::Path;

use koe_transcribe::{Model, Options};

use crate::error::AppError;

/// Builds a GGUF model from a path.
pub fn model_from_path(path: &Path) -> Result<Model, AppError> {
    let text = path.to_str().ok_or_else(|| {
        AppError::usage(format!(
            "model path `{}` is not valid UTF-8",
            path.display()
        ))
    })?;
    Ok(Model::gguf(text))
}

/// Builds session options from an optional language tag.
pub fn options(language: Option<&str>) -> Options {
    language.map_or_else(Options::new, |language| Options::new().language(language))
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use std::path::PathBuf;

    use super::*;

    #[test]
    fn a_model_path_becomes_a_gguf_model() {
        assert_eq!(
            model_from_path(Path::new("/models/tiny.gguf")).unwrap(),
            Model::gguf("/models/tiny.gguf")
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_non_utf8_model_path_is_a_usage_error() {
        use std::ffi::OsString;
        use std::os::unix::ffi::OsStringExt;

        let path = PathBuf::from(OsString::from_vec(vec![0xFF]));
        let error = model_from_path(&path).unwrap_err();
        assert!(matches!(&error, AppError::Usage(_)));
        assert_eq!(error.exit_code(), 2);
    }

    #[test]
    fn options_keep_a_requested_language() {
        assert_eq!(options(None).language_tag(), None);
        assert_eq!(options(Some("pt-BR")).language_tag(), Some("pt-BR"));
    }
}
