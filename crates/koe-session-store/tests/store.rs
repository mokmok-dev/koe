//! End-to-end coverage for the meeting store.

#![allow(clippy::unwrap_used)]

use std::fs;
use std::io::ErrorKind;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use koe_capture_core::Source;
use koe_session::{
    Input, Permission, Permissions, SegmentOutcome, SegmentResult, Session, SourceSet, Stage,
};
use koe_session_store::{DecodeError, Error, Meeting, SessionId, Store, Transcripts};
use koe_transcribe_core::{Segment, Transcript};

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

fn created_at() -> SystemTime {
    UNIX_EPOCH + Duration::from_millis(1_234_567)
}

fn drive(session: Session, inputs: &[Input]) -> Session {
    inputs.iter().fold(session, |session, input| {
        session.step(*input).unwrap().session
    })
}

const fn segment(source: Source, committed: bool) -> Input {
    Input::SegmentClosed {
        source,
        result: SegmentResult {
            outcome: SegmentOutcome::Complete,
            committed,
        },
    }
}

fn recording_session() -> Session {
    let session = drive(
        Session::new(granted()),
        &[
            Input::Start(SourceSet::MEETING),
            Input::DeviceOpened(Source::Microphone),
            Input::DeviceOpened(Source::SystemAudio),
        ],
    );
    assert_eq!(session.stage(), Stage::Recording);
    session
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
            segment(Source::Microphone, true),
            segment(Source::SystemAudio, false),
        ],
    );
    assert_eq!(session.stage(), Stage::Paused);
    session
}

fn reviewing_session() -> Session {
    let session = drive(
        Session::new(granted()),
        &[
            Input::Start(SourceSet::MICROPHONE),
            Input::DeviceOpened(Source::Microphone),
            Input::EngineVolatile(Source::Microphone),
            Input::Stop,
            segment(Source::Microphone, true),
        ],
    );
    assert_eq!(session.stage(), Stage::Reviewing);
    session
}

fn confirmed_session() -> Session {
    let session = drive(reviewing_session(), &[Input::Confirm]);
    assert_eq!(session.stage(), Stage::Confirmed);
    session
}

fn microphone_transcript() -> Transcript {
    Transcript::new(
        vec![
            Segment::new(0, 500, "hello"),
            Segment::new(500, 900, "world"),
        ],
        120,
    )
}

fn texts() -> Transcripts {
    let mut transcripts = Transcripts::new();
    transcripts.set(Source::Microphone, Some(microphone_transcript()));
    transcripts
}

fn meeting_dir(dir: &TempDir, id: SessionId) -> PathBuf {
    dir.join("store").join(id.to_string())
}

#[test]
fn create_then_list_returns_the_new_id() {
    let dir = TempDir::new();
    let store = Store::open(dir.join("store")).unwrap();
    let meeting = store.create(created_at()).unwrap();
    assert_eq!(store.list().unwrap(), vec![meeting.id()]);
}

#[test]
fn two_creates_in_the_same_millisecond_get_distinct_ids() {
    let dir = TempDir::new();
    let store = Store::open(dir.join("store")).unwrap();
    let first = store.create(created_at()).unwrap();
    let second = store.create(created_at()).unwrap();
    assert_ne!(first.id(), second.id());
    assert_eq!(first.id().as_millis(), 1_234_567);
    assert_eq!(second.id().as_millis(), 1_234_568);
    assert_eq!(store.list().unwrap(), vec![first.id(), second.id()]);
}

#[test]
fn list_is_sorted_and_ignores_non_meetings() {
    let dir = TempDir::new();
    let root = dir.join("store");
    let store = Store::open(&root).unwrap();
    for name in [
        "00000000000000000010",
        "00000000000000000009",
        "0000000000000000000A",
    ] {
        fs::create_dir(root.join(name)).unwrap();
        fs::write(root.join(name).join("session"), b"").unwrap();
    }
    fs::write(root.join("00000000000000000011"), b"").unwrap();
    fs::create_dir(root.join("123")).unwrap();
    fs::create_dir(root.join("00000000000000000012")).unwrap();
    fs::create_dir_all(root.join("00000000000000000013").join("session")).unwrap();
    assert_eq!(
        store.list().unwrap(),
        vec![SessionId::from_millis(9), SessionId::from_millis(10)]
    );
}

#[test]
fn create_at_a_fixed_time_is_stable() {
    let dir = TempDir::new();
    let store = Store::open(dir.join("store")).unwrap();
    let meeting = store.create(created_at()).unwrap();
    assert_eq!(meeting.id(), SessionId::from_millis(1_234_567));
    assert_eq!(meeting.id().to_string(), "00000000000001234567");
    assert_eq!(meeting.created_at(), created_at());
    assert_eq!(meeting.session(granted()), None);
    assert!(meeting.transcripts().is_empty());
    assert_eq!(meeting.recording(), None);
}

#[test]
fn save_then_load_returns_the_same_meeting() {
    let dir = TempDir::new();
    let store = Store::open(dir.join("store")).unwrap();
    let mut meeting = store.create(created_at()).unwrap();
    let session = paused_session();
    let transcripts = texts();
    meeting.save(&session, &transcripts).unwrap();

    let loaded = store.load(meeting.id()).unwrap().unwrap();
    assert_eq!(loaded.id(), meeting.id());
    let restored = loaded.session(granted()).unwrap();
    assert_eq!(restored.stage(), session.stage());
    assert_eq!(restored.sources(), session.sources());
    assert_eq!(restored.outcome(), session.outcome());
    assert_eq!(restored.transcript(), session.transcript());
    assert_eq!(loaded.transcripts(), &transcripts);
    assert_eq!(loaded.recording(), None);
    assert_eq!(
        fs::read_to_string(meeting_dir(&dir, meeting.id()).join("transcript.txt")).unwrap(),
        "[0..500] microphone: hello\n[500..900] microphone: world\n"
    );
}

#[test]
fn a_restart_returns_the_same_meeting() {
    let dir = TempDir::new();
    let root = dir.join("store");
    let mut meeting = Store::open(&root).unwrap().create(created_at()).unwrap();
    let id = meeting.id();
    let session = paused_session();
    meeting.save(&session, &texts()).unwrap();
    meeting.set_recording("meeting.ogg").unwrap();
    drop(meeting);

    let store = Store::open(&root).unwrap();
    assert_eq!(store.list().unwrap(), vec![id]);
    let loaded = store.load(id).unwrap().unwrap();
    assert_eq!(loaded.id(), id);
    assert_eq!(loaded.created_at(), created_at());
    assert_eq!(loaded.session(granted()), Some(session));
    assert_eq!(loaded.transcripts(), &texts());
    assert_eq!(
        loaded.recording(),
        Some(meeting_dir(&dir, id).join("meeting.ogg"))
    );
}

#[test]
fn permissions_are_supplied_when_restoring() {
    let dir = TempDir::new();
    let store = Store::open(dir.join("store")).unwrap();
    let mut meeting = store.create(created_at()).unwrap();
    meeting.save(&paused_session(), &texts()).unwrap();

    let restricted = Permissions::new(Permission::Granted, Permission::Denied);
    let loaded = store.load(meeting.id()).unwrap().unwrap();
    let restored = loaded.session(restricted).unwrap();
    assert_eq!(restored.permissions(), restricted);
    assert_eq!(restored.permission(Source::Microphone), Permission::Granted);
    assert_eq!(restored.permission(Source::SystemAudio), Permission::Denied);

    let denied = Permissions::new(Permission::Denied, Permission::Denied);
    let restored = store
        .load(meeting.id())
        .unwrap()
        .unwrap()
        .session(denied)
        .unwrap();
    assert_eq!(restored.permissions(), denied);
    assert_eq!(restored.stage(), Stage::Paused);
}

#[test]
fn a_recording_session_reloads_as_paused() {
    let dir = TempDir::new();
    let store = Store::open(dir.join("store")).unwrap();
    let mut meeting = store.create(created_at()).unwrap();
    let session = recording_session();
    meeting.save(&session, &texts()).unwrap();

    let restored = store
        .load(meeting.id())
        .unwrap()
        .unwrap()
        .session(granted())
        .unwrap();
    assert_eq!(restored.stage(), Stage::Paused);
    assert_eq!(restored.sources(), Some(SourceSet::MEETING));
    assert_eq!(restored.transcript(), session.transcript());
}

#[test]
fn a_confirming_session_reloads_as_confirmed_with_text() {
    let dir = TempDir::new();
    let store = Store::open(dir.join("store")).unwrap();
    let mut meeting = store.create(created_at()).unwrap();
    meeting.save(&confirmed_session(), &texts()).unwrap();

    let loaded = store.load(meeting.id()).unwrap().unwrap();
    assert_eq!(loaded.session(granted()).unwrap().stage(), Stage::Confirmed);
    assert_eq!(
        loaded.transcripts().get(Source::Microphone).unwrap().text(),
        "hello world"
    );
    assert_eq!(loaded.transcripts().get(Source::SystemAudio), None);
}

#[test]
fn a_fresh_preparing_session_keeps_the_confirmed_checkpoint() {
    let dir = TempDir::new();
    let store = Store::open(dir.join("store")).unwrap();
    let mut meeting = store.create(created_at()).unwrap();
    meeting.save(&confirmed_session(), &texts()).unwrap();

    let preparing = drive(
        Session::new(granted()),
        &[Input::Start(SourceSet::MICROPHONE)],
    );
    assert_eq!(preparing.stage(), Stage::Preparing);
    meeting.save(&preparing, &Transcripts::new()).unwrap();

    let loaded = store.load(meeting.id()).unwrap().unwrap();
    assert_eq!(loaded.session(granted()).unwrap().stage(), Stage::Confirmed);
    assert_eq!(loaded.transcripts(), &Transcripts::new());
}

#[test]
fn absent_and_empty_transcripts_stay_distinct() {
    let dir = TempDir::new();
    let store = Store::open(dir.join("store")).unwrap();
    let mut meeting = store.create(created_at()).unwrap();
    let session = paused_session();

    meeting.save(&session, &Transcripts::new()).unwrap();
    let loaded = store.load(meeting.id()).unwrap().unwrap();
    assert_eq!(loaded.transcripts().get(Source::Microphone), None);
    assert!(loaded.transcripts().is_empty());

    let empty = Transcript::new(Vec::new(), 0);
    let mut transcripts = Transcripts::new();
    transcripts.set(Source::Microphone, Some(empty.clone()));
    meeting.save(&session, &transcripts).unwrap();
    let loaded = store.load(meeting.id()).unwrap().unwrap();
    assert_eq!(loaded.transcripts().get(Source::Microphone), Some(&empty));
    assert!(!loaded.transcripts().is_empty());
    assert_eq!(loaded.transcripts().get(Source::SystemAudio), None);
}

#[test]
fn save_transcripts_writes_the_record_and_the_text_file() {
    let dir = TempDir::new();
    let store = Store::open(dir.join("store")).unwrap();
    let mut meeting = store.create(created_at()).unwrap();
    let session = paused_session();
    meeting.save(&session, &Transcripts::new()).unwrap();

    let mut transcripts = Transcripts::new();
    transcripts.set(Source::Microphone, Some(microphone_transcript()));
    transcripts.set(
        Source::SystemAudio,
        Some(Transcript::new(vec![Segment::new(200, 700, "remote")], 0)),
    );
    meeting.save_transcripts(&transcripts).unwrap();

    let directory = meeting_dir(&dir, meeting.id());
    assert_eq!(
        fs::read_to_string(directory.join("transcript.txt")).unwrap(),
        "[0..500] microphone: hello\n\
         [200..700] system audio: remote\n\
         [500..900] microphone: world\n"
    );
    let loaded = store.load(meeting.id()).unwrap().unwrap();
    assert_eq!(loaded.transcripts(), &transcripts);
    assert_eq!(loaded.session(granted()), Some(session));
}

#[test]
fn save_transcripts_keeps_the_recorded_audio_name() {
    let dir = TempDir::new();
    let store = Store::open(dir.join("store")).unwrap();
    let mut meeting = store.create(created_at()).unwrap();
    meeting.set_recording("meeting.ogg").unwrap();
    meeting.save_transcripts(&texts()).unwrap();

    let loaded = store.load(meeting.id()).unwrap().unwrap();
    assert_eq!(
        loaded.recording(),
        Some(meeting_dir(&dir, meeting.id()).join("meeting.ogg"))
    );
    assert_eq!(loaded.transcripts(), &texts());
}

#[test]
fn saving_an_empty_set_empties_the_text_file() {
    let dir = TempDir::new();
    let store = Store::open(dir.join("store")).unwrap();
    let mut meeting = store.create(created_at()).unwrap();
    meeting.save_transcripts(&texts()).unwrap();
    let path = meeting_dir(&dir, meeting.id()).join("transcript.txt");
    assert_ne!(fs::read_to_string(&path).unwrap(), "");

    meeting.save_transcripts(&Transcripts::new()).unwrap();
    assert_eq!(fs::read_to_string(&path).unwrap(), "");
    assert_eq!(
        store.load(meeting.id()).unwrap().unwrap().transcripts(),
        &Transcripts::new()
    );
}

#[test]
fn a_saved_transcript_replaces_the_previous_text_and_leaves_no_partial() {
    let dir = TempDir::new();
    let store = Store::open(dir.join("store")).unwrap();
    let mut meeting = store.create(created_at()).unwrap();
    meeting.save_transcripts(&texts()).unwrap();

    let shorter = Transcript::new(vec![Segment::new(0, 10, "hi")], 0);
    let mut transcripts = Transcripts::new();
    transcripts.set(Source::Microphone, Some(shorter));
    meeting.save_transcripts(&transcripts).unwrap();

    let directory = meeting_dir(&dir, meeting.id());
    assert_eq!(
        fs::read_to_string(directory.join("transcript.txt")).unwrap(),
        "[0..10] microphone: hi\n"
    );
    assert!(!directory.join("transcript.partial").exists());
}

#[test]
fn an_oversized_transcript_is_an_error_and_keeps_the_previous_text() {
    let dir = TempDir::new();
    let store = Store::open(dir.join("store")).unwrap();
    let mut meeting = store.create(created_at()).unwrap();
    meeting.save_transcripts(&texts()).unwrap();
    let directory = meeting_dir(&dir, meeting.id());
    let text = fs::read_to_string(directory.join("transcript.txt")).unwrap();

    let oversized = Transcript::new(vec![Segment::new(0, 1, "x".repeat(4 * 1024 * 1024))], 0);
    let mut transcripts = Transcripts::new();
    transcripts.set(Source::Microphone, Some(oversized));
    assert!(matches!(
        meeting.save_transcripts(&transcripts),
        Err(Error::TextTooLarge { .. })
    ));
    assert_eq!(
        fs::read_to_string(directory.join("transcript.txt")).unwrap(),
        text
    );
    assert_eq!(
        store.load(meeting.id()).unwrap().unwrap().transcripts(),
        &texts()
    );
}

#[test]
fn a_text_that_cannot_be_written_leaves_the_record_holding_the_transcripts() {
    let dir = TempDir::new();
    let store = Store::open(dir.join("store")).unwrap();
    let mut meeting = store.create(created_at()).unwrap();
    let directory = meeting_dir(&dir, meeting.id());
    fs::create_dir(directory.join("transcript.partial")).unwrap();

    assert!(matches!(
        meeting.save_transcripts(&texts()),
        Err(Error::TextNotWritten { .. })
    ));
    let loaded = store.load(meeting.id()).unwrap().unwrap();
    assert_eq!(loaded.transcripts(), &texts());
}

#[test]
fn set_recording_names_the_audio_file() {
    let dir = TempDir::new();
    let root = dir.join("store");
    let store = Store::open(&root).unwrap();
    let mut meeting = store.create(created_at()).unwrap();
    let id = meeting.id();
    let path = meeting_dir(&dir, id).join("meeting.ogg");

    assert_eq!(meeting.set_recording("meeting.ogg").unwrap(), path);
    assert_eq!(meeting.recording(), Some(path.clone()));
    meeting.set_recording("meeting.ogg").unwrap();

    fs::write(&path, b"audio").unwrap();
    let other = meeting_dir(&dir, id).join("other.ogg");
    assert_eq!(meeting.set_recording("other.ogg").unwrap(), other);
    assert_eq!(meeting.recording(), Some(other.clone()));
    assert!(path.exists());

    drop(store);
    let store = Store::open(&root).unwrap();
    let loaded = store.load(id).unwrap().unwrap();
    assert_eq!(loaded.recording(), Some(other));
}

#[test]
fn a_save_keeps_the_recorded_audio_name() {
    let dir = TempDir::new();
    let store = Store::open(dir.join("store")).unwrap();
    let mut meeting = store.create(created_at()).unwrap();
    meeting.set_recording("meeting.ogg").unwrap();
    meeting.save(&paused_session(), &texts()).unwrap();

    let loaded = store.load(meeting.id()).unwrap().unwrap();
    assert_eq!(
        loaded.recording(),
        Some(meeting_dir(&dir, meeting.id()).join("meeting.ogg"))
    );
    assert_eq!(loaded.transcripts(), &texts());
}

#[test]
fn every_invalid_recording_name_is_rejected() {
    let dir = TempDir::new();
    let store = Store::open(dir.join("store")).unwrap();
    let mut meeting = store.create(created_at()).unwrap();
    meeting.set_recording("meeting.ogg").unwrap();

    let long = "x".repeat(256);
    for name in [
        "",
        ".",
        "..",
        ".hidden",
        "a/b.ogg",
        "a\\b.ogg",
        "a\0b.ogg",
        "session",
        "SESSION",
        "session ",
        "session.partial",
        "transcript.txt",
        "Transcript.txt",
        "transcript.txt.",
        "transcript.partial",
        long.as_str(),
    ] {
        assert_eq!(
            meeting.set_recording(name),
            Err(Error::InvalidRecording),
            "{name:?}"
        );
    }
    assert_eq!(
        meeting.recording(),
        Some(meeting_dir(&dir, meeting.id()).join("meeting.ogg"))
    );
}

#[test]
fn a_stale_partial_is_ignored_and_replaced() {
    let dir = TempDir::new();
    let store = Store::open(dir.join("store")).unwrap();
    let mut meeting = store.create(created_at()).unwrap();
    let partial = meeting_dir(&dir, meeting.id()).join("session.partial");
    fs::write(&partial, [0xFFu8; 32]).unwrap();

    let session = paused_session();
    meeting.save(&session, &texts()).unwrap();
    assert!(!partial.exists());
    assert_eq!(
        store
            .load(meeting.id())
            .unwrap()
            .unwrap()
            .session(granted()),
        Some(session)
    );
}

#[test]
fn a_partial_directory_makes_save_fail_and_keeps_the_old_record() {
    let dir = TempDir::new();
    let store = Store::open(dir.join("store")).unwrap();
    let mut meeting = store.create(created_at()).unwrap();
    let paused = paused_session();
    meeting.save(&paused, &texts()).unwrap();

    fs::create_dir(meeting_dir(&dir, meeting.id()).join("session.partial")).unwrap();
    assert!(matches!(
        meeting.save(&confirmed_session(), &texts()),
        Err(Error::Io { .. })
    ));

    let loaded = store.load(meeting.id()).unwrap().unwrap();
    assert_eq!(loaded.session(granted()).unwrap().stage(), Stage::Paused);
    assert_eq!(loaded.transcripts(), &texts());
}

#[test]
fn discard_removes_the_meeting_and_is_idempotent() {
    let dir = TempDir::new();
    let store = Store::open(dir.join("store")).unwrap();
    let mut meeting = store.create(created_at()).unwrap();
    let id = meeting.id();
    meeting.save(&paused_session(), &texts()).unwrap();

    store.discard(id).unwrap();
    assert!(!meeting_dir(&dir, id).exists());
    assert_eq!(store.list().unwrap(), []);
    assert!(store.load(id).unwrap().is_none());
    store.discard(id).unwrap();
}

#[test]
fn open_sweeps_staging_and_trash() {
    let dir = TempDir::new();
    let root = dir.join("store");
    fs::create_dir_all(&root).unwrap();
    fs::create_dir(root.join(".new-00000000000000000001")).unwrap();
    fs::write(root.join(".new-00000000000000000001").join("session"), b"x").unwrap();
    fs::create_dir(root.join(".trash-00000000000000000002")).unwrap();
    fs::create_dir(root.join("00000000000000000003")).unwrap();
    fs::write(root.join("00000000000000000003").join("session"), b"x").unwrap();

    let store = Store::open(&root).unwrap();
    assert!(!root.join(".new-00000000000000000001").exists());
    assert!(!root.join(".trash-00000000000000000002").exists());
    assert!(root.join("00000000000000000003").exists());
    assert_eq!(store.list().unwrap(), vec![SessionId::from_millis(3)]);
}

#[test]
fn loading_a_missing_meeting_is_none() {
    let dir = TempDir::new();
    let store = Store::open(dir.join("store")).unwrap();
    assert!(store.load(SessionId::from_millis(42)).unwrap().is_none());
}

#[test]
fn an_oversized_record_is_reported_as_too_long() {
    let dir = TempDir::new();
    let store = Store::open(dir.join("store")).unwrap();
    let meeting = store.create(created_at()).unwrap();
    fs::write(
        meeting_dir(&dir, meeting.id()).join("session"),
        vec![0u8; 10 * 1024 * 1024],
    )
    .unwrap();

    assert!(matches!(
        store.load(meeting.id()),
        Err(Error::Decode(DecodeError::TooLong { .. }))
    ));
}

#[cfg(unix)]
#[test]
fn a_meeting_path_that_cannot_be_opened_is_an_io_error() {
    let dir = TempDir::new();
    let store = Store::open(dir.join("store")).unwrap();
    let id = SessionId::from_millis(7);
    fs::write(dir.join("store").join(id.to_string()), b"not a directory").unwrap();

    assert!(matches!(store.load(id), Err(Error::Io { .. })));
}

#[cfg(unix)]
#[test]
fn discard_reports_a_rename_error() {
    use std::os::unix::fs::PermissionsExt;

    let dir = TempDir::new();
    let root = dir.join("store");
    let store = Store::open(&root).unwrap();
    let meeting = store.create(created_at()).unwrap();
    let id = meeting.id();

    let original = fs::metadata(&root).unwrap().permissions().mode();
    let mut read_only = fs::metadata(&root).unwrap().permissions();
    read_only.set_mode(0o555);
    fs::set_permissions(&root, read_only).unwrap();

    let result = store.discard(id);

    let mut restored = fs::metadata(&root).unwrap().permissions();
    restored.set_mode(original);
    fs::set_permissions(&root, restored).unwrap();

    assert!(matches!(result, Err(Error::Io { .. })));
}

#[cfg(unix)]
#[test]
fn a_trash_copy_that_cannot_be_removed_still_discards_the_meeting() {
    use std::os::unix::fs::PermissionsExt;

    let dir = TempDir::new();
    let root = dir.join("store");
    let store = Store::open(&root).unwrap();
    let meeting = store.create(created_at()).unwrap();
    let id = meeting.id();
    let directory = meeting_dir(&dir, id);
    fs::write(directory.join("kept"), b"x").unwrap();

    // `remove_dir_all` needs write permission on every directory it empties.
    let original = fs::metadata(&directory).unwrap().permissions().mode();
    let mut read_only = fs::metadata(&directory).unwrap().permissions();
    read_only.set_mode(0o555);
    fs::set_permissions(&directory, read_only).unwrap();

    let result = store.discard(id);

    let trash = root.join(format!(".trash-{id}"));
    let mut restored = fs::metadata(&trash).unwrap().permissions();
    restored.set_mode(original);
    fs::set_permissions(&trash, restored).unwrap();

    assert_eq!(result, Ok(()));
    assert!(!directory.exists());
    assert_eq!(store.list().unwrap(), []);
}

#[cfg(unix)]
#[test]
fn a_stale_trash_copy_that_cannot_be_removed_fails_the_discard() {
    use std::os::unix::fs::PermissionsExt;

    let dir = TempDir::new();
    let root = dir.join("store");
    let store = Store::open(&root).unwrap();
    let meeting = store.create(created_at()).unwrap();
    let id = meeting.id();
    let directory = meeting_dir(&dir, id);

    let trash = root.join(format!(".trash-{id}"));
    fs::create_dir(&trash).unwrap();
    fs::write(trash.join("kept"), b"x").unwrap();
    let original = fs::metadata(&trash).unwrap().permissions().mode();
    let mut read_only = fs::metadata(&trash).unwrap().permissions();
    read_only.set_mode(0o555);
    fs::set_permissions(&trash, read_only).unwrap();

    let result = store.discard(id);

    let mut restored = fs::metadata(&trash).unwrap().permissions();
    restored.set_mode(original);
    fs::set_permissions(&trash, restored).unwrap();

    assert!(
        matches!(
            result,
            Err(Error::Io {
                operation: "remove",
                ..
            })
        ),
        "{result:?}"
    );
    assert!(directory.exists());
    assert_eq!(store.list().unwrap(), vec![id]);
}

#[test]
fn a_corrupt_record_is_a_decode_error_and_still_lists() {
    let dir = TempDir::new();
    let store = Store::open(dir.join("store")).unwrap();
    let meeting = store.create(created_at()).unwrap();
    fs::write(meeting_dir(&dir, meeting.id()).join("session"), b"garbage").unwrap();

    assert_eq!(store.list().unwrap(), vec![meeting.id()]);
    assert!(matches!(
        store.load(meeting.id()),
        Err(Error::Decode(DecodeError::Length { len: 7 }))
    ));
}

#[test]
fn the_store_is_send_and_sync_and_the_meeting_is_send() {
    fn send_sync<T: Send + Sync>() {}
    fn send<T: Send>() {}
    send_sync::<Store>();
    send::<Meeting>();
}

#[test]
fn a_decode_error_is_the_source_of_its_store_error() {
    use std::error::Error as _;

    let decode = Error::Decode(DecodeError::TooLong { len: 9 });
    assert!(decode.source().is_some());

    let io = Error::Io {
        operation: "read",
        kind: ErrorKind::NotFound,
    };
    assert!(io.source().is_none());

    let text = Error::TextNotWritten {
        source: Box::new(Error::IdCollision),
    };
    assert!(text.source().is_some());
}

#[test]
fn display_names_every_error_variant() {
    let errors = [
        (
            Error::Io {
                operation: "create",
                kind: ErrorKind::NotFound,
            },
            "session store `create` failed: entity not found",
        ),
        (
            Error::Decode(DecodeError::TooLong { len: 9 }),
            "session record is not a record this build wrote: a record is at most 8388905 bytes, not 9",
        ),
        (
            Error::IdCollision,
            "every meeting id within 16 milliseconds of the requested time is taken",
        ),
        (
            Error::TextTooLarge {
                source: Source::Microphone,
                bytes: 4_194_305,
            },
            "microphone transcript does not fit a record: 4194305 bytes; a record holds at most 4194304",
        ),
        (
            Error::RecordTooLarge { bytes: 9 },
            "a record of 9 bytes cannot be written; the format holds at most 8388905",
        ),
        (
            Error::InvalidRecording,
            "a recording name must be one plain file name in the meeting directory that the store does not write itself",
        ),
        (
            Error::TextNotWritten {
                source: Box::new(Error::Io {
                    operation: "write transcript",
                    kind: ErrorKind::NotFound,
                }),
            },
            "transcript.txt was not written: session store `write transcript` failed: entity not found",
        ),
    ];
    for (error, text) in errors {
        assert_eq!(error.to_string(), text);
    }
}

#[test]
fn display_names_every_decode_error() {
    let errors = [
        (
            DecodeError::TooLong { len: 9 },
            "a record is at most 8388905 bytes, not 9",
        ),
        (
            DecodeError::Length { len: 3 },
            "a record is at least 42 bytes, not 3",
        ),
        (
            DecodeError::Magic { found: [0; 4] },
            "a record starts with KOEM, not [0, 0, 0, 0]",
        ),
        (
            DecodeError::Version { found: 2 },
            "record version 2 is unknown",
        ),
        (
            DecodeError::Flags { flags: 4 },
            "record flags 0x04 carry bits this build does not write",
        ),
        (
            DecodeError::Reserved { offset: 22 },
            "record byte 22 is reserved and must be zero",
        ),
        (
            DecodeError::Checksum {
                expected: 0x1234_5678,
                found: 0x9ABC_DEF0,
            },
            "checksum 0x9abcdef0 does not cover the bytes; expected 0x12345678",
        ),
        (
            DecodeError::TotalLength {
                declared: 42,
                actual: 41,
            },
            "a record declares 42 bytes but holds 41",
        ),
        (
            DecodeError::Checkpoint(koe_session::DecodeError::MissingFault),
            "the checkpoint is not one this build wrote: an incomplete outcome names no fault",
        ),
        (
            DecodeError::UnexpectedCheckpoint,
            "a record without a checkpoint carries checkpoint bytes",
        ),
        (
            DecodeError::UnexpectedRecording,
            "a record without a recording carries an audio file name",
        ),
        (
            DecodeError::RecordingName { len: 0 },
            "a recording name of 0 bytes is not one plain file name",
        ),
        (
            DecodeError::ReservedName { len: 14 },
            "a recording name of 14 bytes is reserved for a file the store writes itself",
        ),
        (
            DecodeError::SectionLength {
                source: Source::Microphone,
                len: 5,
            },
            "microphone transcript section of 5 bytes is not 12..=4194304",
        ),
        (
            DecodeError::SegmentCount {
                source: Source::SystemAudio,
                count: 3,
                len: 12,
            },
            "system audio transcript section of 12 bytes cannot hold 3 segments of 20 bytes",
        ),
        (
            DecodeError::Segment {
                source: Source::Microphone,
                index: 1,
            },
            "microphone transcript segment 1 runs past its section",
        ),
        (
            DecodeError::Text {
                source: Source::SystemAudio,
                index: 2,
            },
            "system audio transcript segment 2 is not UTF-8 text",
        ),
        (
            DecodeError::Section {
                source: Source::Microphone,
            },
            "microphone transcript section has bytes left over",
        ),
    ];
    for (error, text) in errors {
        assert_eq!(error.to_string(), text);
    }
}
