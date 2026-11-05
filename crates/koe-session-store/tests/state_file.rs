//! End-to-end coverage for the session state file.

#![allow(clippy::unwrap_used)]

use std::fs;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use koe_session::{
    Checkpoint, DecodeError, Input, Permission, Permissions, SegmentOutcome, SegmentResult,
    Session, Source, SourceSet, Stage,
};
use koe_session_store::{Error, StateFile};

struct TempDir {
    path: PathBuf,
}

impl TempDir {
    fn new() -> Self {
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let serial = COUNTER.fetch_add(1, Ordering::Relaxed);
        let path =
            std::env::temp_dir().join(format!("koe-session-store-{}-{serial}", std::process::id()));
        let _ = fs::remove_dir_all(&path);
        fs::create_dir_all(&path).unwrap();
        Self { path }
    }

    fn join(&self, name: &str) -> PathBuf {
        self.path.join(name)
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}

const fn granted() -> Permissions {
    Permissions::new(Permission::Granted, Permission::Granted)
}

fn drive(session: Session, inputs: &[Input]) -> Session {
    inputs.iter().fold(session, |session, input| {
        session.step(*input).unwrap().session
    })
}

const fn segment(source: Source, outcome: SegmentOutcome, committed: bool) -> Input {
    Input::SegmentClosed {
        source,
        result: SegmentResult { outcome, committed },
    }
}

fn paused_session() -> Session {
    let session = drive(
        Session::new(granted()),
        &[
            Input::Start(SourceSet::MEETING),
            Input::DeviceOpened(Source::Microphone),
            Input::DeviceOpened(Source::SystemAudio),
            Input::EngineCommitted(Source::Microphone),
            Input::CaptureFailed(Source::SystemAudio),
            Input::Pause,
            segment(Source::Microphone, SegmentOutcome::Complete, true),
            segment(Source::SystemAudio, SegmentOutcome::Complete, false),
        ],
    );
    assert_eq!(session.stage(), Stage::Paused);
    session
}

fn reviewed_session() -> Session {
    let session = drive(
        Session::new(granted()),
        &[
            Input::Start(SourceSet::MICROPHONE),
            Input::DeviceOpened(Source::Microphone),
            Input::EngineVolatile(Source::Microphone),
            Input::Stop,
            segment(Source::Microphone, SegmentOutcome::Complete, true),
        ],
    );
    assert_eq!(session.stage(), Stage::Reviewing);
    session
}

fn partial_path(path: &Path) -> PathBuf {
    PathBuf::from(format!("{}.partial", path.display()))
}

#[test]
fn save_then_load_returns_the_same_session() {
    let dir = TempDir::new();
    let file = StateFile::new(dir.join("state.bin"));
    let session = paused_session();
    file.save(&session).unwrap();
    assert_eq!(file.load(granted()).unwrap(), Some(session));
}

#[test]
fn loading_uses_the_permissions_given_now() {
    let dir = TempDir::new();
    let file = StateFile::new(dir.join("state.bin"));
    let session = paused_session();
    file.save(&session).unwrap();
    let permissions = Permissions::new(Permission::Granted, Permission::Denied);
    let loaded = file.load(permissions).unwrap().unwrap();
    assert_eq!(loaded.permissions(), permissions);
    assert_eq!(loaded.permission(Source::Microphone), Permission::Granted);
    assert_eq!(loaded.permission(Source::SystemAudio), Permission::Denied);
    assert_eq!(loaded.stage(), session.stage());
    assert_eq!(loaded.sources(), session.sources());
    assert_eq!(loaded.transcript(), session.transcript());
    assert_eq!(loaded.outcome(), session.outcome());
    assert!(loaded.open_sources().is_empty());
    assert!(loaded.pending_sources().is_empty());
    assert!(loaded.finishing_sources().is_empty());
}

#[test]
fn save_then_load_returns_the_same_session_under_denied_permissions() {
    let dir = TempDir::new();
    let file = StateFile::new(dir.join("state.bin"));
    let session = paused_session();
    file.save(&session).unwrap();
    let permissions = Permissions::new(Permission::Denied, Permission::Denied);
    let loaded = file.load(permissions).unwrap().unwrap();
    assert_eq!(loaded.permissions(), permissions);
    assert_eq!(loaded.stage(), session.stage());
    assert_eq!(loaded.sources(), session.sources());
    assert_eq!(loaded.transcript(), session.transcript());
    assert_eq!(loaded.outcome(), session.outcome());
}

#[test]
fn saving_over_an_existing_file_replaces_it() {
    let dir = TempDir::new();
    let file = StateFile::new(dir.join("state.bin"));
    file.save(&paused_session()).unwrap();
    let replacement = reviewed_session();
    file.save(&replacement).unwrap();
    assert_eq!(file.load(granted()).unwrap(), Some(replacement));
}

#[test]
fn saving_an_idle_session_removes_the_file() {
    let dir = TempDir::new();
    let path = dir.join("state.bin");
    let file = StateFile::new(&path);
    file.save(&paused_session()).unwrap();
    assert!(path.exists());
    file.save(&Session::new(granted())).unwrap();
    assert!(!path.exists());
    assert_eq!(file.load(granted()).unwrap(), None);
}

#[test]
fn saving_an_idle_session_without_a_file_succeeds() {
    let dir = TempDir::new();
    let path = dir.join("state.bin");
    let file = StateFile::new(&path);
    file.save(&Session::new(granted())).unwrap();
    assert!(!path.exists());
    assert_eq!(file.load(granted()).unwrap(), None);
}

#[test]
fn loading_a_missing_file_is_none() {
    let dir = TempDir::new();
    let file = StateFile::new(dir.join("state.bin"));
    assert_eq!(file.load(granted()).unwrap(), None);
}

#[test]
fn a_corrupt_file_is_a_decode_error() {
    let dir = TempDir::new();
    let path = dir.join("state.bin");
    fs::write(&path, [0u8; 14]).unwrap();
    let file = StateFile::new(&path);
    assert_eq!(
        file.load(granted()),
        Err(Error::Decode(DecodeError::Magic { found: [0; 4] }))
    );
}

#[test]
fn a_truncated_file_is_a_decode_error() {
    let dir = TempDir::new();
    let path = dir.join("state.bin");
    let image = Checkpoint::of(&paused_session()).unwrap().to_bytes();
    fs::write(&path, &image[..5]).unwrap();
    let file = StateFile::new(&path);
    assert_eq!(
        file.load(granted()),
        Err(Error::Decode(DecodeError::Length { len: 5 }))
    );
}

#[test]
fn an_oversized_file_is_read_only_up_to_the_bound() {
    let dir = TempDir::new();
    let path = dir.join("state.bin");
    fs::write(&path, vec![0xAAu8; 1_000]).unwrap();
    let file = StateFile::new(&path);
    assert_eq!(
        file.load(granted()),
        Err(Error::Decode(DecodeError::Length { len: 15 }))
    );
}

#[test]
fn a_save_leaves_no_partial_file() {
    let dir = TempDir::new();
    let path = dir.join("state.bin");
    let file = StateFile::new(&path);
    file.save(&paused_session()).unwrap();
    assert!(path.exists());
    assert!(!partial_path(&path).exists());
}

#[test]
fn a_stale_partial_file_is_replaced_and_never_read() {
    let dir = TempDir::new();
    let path = dir.join("state.bin");
    let stale = partial_path(&path);
    fs::write(&stale, [0xFFu8; 20]).unwrap();
    let file = StateFile::new(&path);
    let session = paused_session();
    file.save(&session).unwrap();
    assert!(!stale.exists());
    assert_eq!(file.load(granted()).unwrap(), Some(session));
}

#[test]
fn a_save_under_a_missing_directory_is_an_io_error() {
    let dir = TempDir::new();
    let file = StateFile::new(dir.join("missing").join("state.bin"));
    assert_eq!(
        file.save(&paused_session()),
        Err(Error::Io {
            operation: "create",
            kind: ErrorKind::NotFound,
        })
    );
}

#[test]
fn removing_a_state_file_that_is_a_directory_is_an_io_error() {
    let dir = TempDir::new();
    let path = dir.join("state.bin");
    fs::create_dir(&path).unwrap();
    let file = StateFile::new(&path);
    assert!(matches!(
        file.save(&Session::new(granted())),
        Err(Error::Io {
            operation: "remove",
            ..
        })
    ));
}

#[test]
fn loading_a_directory_is_an_io_error() {
    let dir = TempDir::new();
    let path = dir.join("state.bin");
    fs::create_dir(&path).unwrap();
    let file = StateFile::new(&path);
    let result = file.load(granted());
    assert!(matches!(&result, Err(Error::Io { .. })));
    #[cfg(unix)]
    assert!(matches!(
        &result,
        Err(Error::Io {
            operation: "read",
            ..
        })
    ));
}

#[cfg(unix)]
#[test]
fn an_unreadable_file_is_an_open_error() {
    use std::os::unix::fs::PermissionsExt;

    let dir = TempDir::new();
    let path = dir.join("state.bin");
    fs::write(&path, b"").unwrap();
    fs::set_permissions(&path, fs::Permissions::from_mode(0o000)).unwrap();
    if fs::File::open(&path).is_ok() {
        return;
    }
    let file = StateFile::new(&path);
    assert!(matches!(
        file.load(granted()),
        Err(Error::Io {
            operation: "open",
            kind: ErrorKind::PermissionDenied,
        })
    ));
}

#[test]
fn display_names_every_error_variant() {
    assert_eq!(
        Error::Io {
            operation: "create",
            kind: ErrorKind::NotFound,
        }
        .to_string(),
        "session state `create` failed: entity not found"
    );
    assert_eq!(
        Error::Decode(DecodeError::MissingFault).to_string(),
        "session state is not a checkpoint: an incomplete outcome names no fault"
    );
}
