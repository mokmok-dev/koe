//! The dictation thread: microphone → transcript → keystrokes.
//!
//! One thread owns the microphone, the streaming transcription, and the text
//! injection, so the UI never waits on inference or on the keyboard. The
//! model loads on a second thread while the microphone is already capturing,
//! and the audio that arrives first waits in a bounded backlog, so the first
//! words are not lost to the load. As with the recorder, a stalled device
//! delays the save, not the UI.
//!
//! A stop commits: the transcript is inserted into the focused app. A cancel
//! discards: nothing is inserted. The final inference cannot be interrupted
//! mid-way, but a cancel that lands while it runs still wins the race against
//! the insertion.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Sender, TryRecvError};
use std::sync::{Mutex, PoisonError};
use std::thread;
#[cfg(target_os = "linux")]
use std::time::Duration;

use koe_capture::{Source, StreamSpec};
use koe_transcribe::{AudioSpec, LiveSession, Options, Transcriber};

use crate::dictation::{DictationOutcome, DictationPlan};
use crate::engine::SAMPLE_RATE;
use crate::error::DesktopError;

/// Frames each microphone read delivers, one mono channel wide.
const BLOCK_FRAMES: usize = 960;

/// Samples the backlog may hold while the model loads: about a minute.
///
/// The backlog only grows during the load, so this ceiling is the load time
/// the thread is willing to buffer; the oldest samples go first once it is
/// full.
const BACKLOG_LIMIT: usize = SAMPLE_RATE as usize * 60;

/// How long the clipboard takes to settle before the paste shortcut is sent,
/// so the focused app reads the new selection and not the previous one.
#[cfg(target_os = "linux")]
const PASTE_SETTLE: Duration = Duration::from_millis(100);

/// How long after the paste the previous clipboard comes back. Nothing
/// acknowledges that the target read the paste, so the wait is a bounded
/// settling policy, the same one hex, the reference dictation app, uses.
#[cfg(target_os = "linux")]
const RESTORE_DELAY: Duration = Duration::from_millis(500);

/// One model load at a time. Rapid stop-and-restart cycles queue here instead
/// of stacking concurrent loads, each of which holds a model that can run to
/// hundreds of megabytes.
static MODEL_LOAD: Mutex<()> = Mutex::new(());

/// A running dictation.
pub struct Session {
    stop: Arc<AtomicBool>,
    cancel: Arc<AtomicBool>,
    events: mpsc::Receiver<Box<DictationOutcome>>,
}

impl Session {
    /// Starts the dictation thread for `plan`.
    ///
    /// # Errors
    ///
    /// Returns [`DesktopError::Thread`] when the thread cannot be spawned.
    pub fn start(plan: DictationPlan) -> Result<Self, DesktopError> {
        let (sender, events) = mpsc::channel();
        let stop = Arc::new(AtomicBool::new(false));
        let cancel = Arc::new(AtomicBool::new(false));
        let thread_stop = Arc::clone(&stop);
        let thread_cancel = Arc::clone(&cancel);
        thread::Builder::new()
            .name("koe-dictation".to_owned())
            .spawn(move || run(&plan, &thread_stop, &thread_cancel, &sender))
            .map_err(|error| DesktopError::Thread(error.to_string()))?;
        Ok(Self {
            stop,
            cancel,
            events,
        })
    }

    /// Asks the microphone and the transcription to stop; the transcript is
    /// still inserted.
    pub fn stop(&self) {
        self.stop.store(true, Ordering::Release);
    }

    /// Asks the dictation to stop and discard; nothing is inserted.
    pub fn cancel(&self) {
        self.cancel.store(true, Ordering::Release);
        self.stop();
    }

    /// Takes the thread's report without blocking.
    #[must_use]
    pub fn poll(&self) -> Report {
        match self.events.try_recv() {
            Ok(outcome) => Report::Done(outcome),
            Err(TryRecvError::Empty) => Report::Pending,
            Err(TryRecvError::Disconnected) => Report::Lost,
        }
    }
}

/// What the dictation thread has to say.
#[derive(Debug)]
pub enum Report {
    /// The thread is still running.
    Pending,
    /// The outcome the thread produced.
    Done(Box<DictationOutcome>),
    /// The thread ended without producing an outcome.
    Lost,
}

impl Drop for Session {
    fn drop(&mut self) {
        self.stop();
    }
}

fn run(
    plan: &DictationPlan,
    stop: &Arc<AtomicBool>,
    cancel: &Arc<AtomicBool>,
    sender: &Sender<Box<DictationOutcome>>,
) {
    let outcome = capture(plan, stop, cancel);
    let _ = sender.send(Box::new(outcome));
}

/// Captures the microphone, transcribes it, and inserts the result.
fn capture(
    plan: &DictationPlan,
    stop: &Arc<AtomicBool>,
    cancel: &Arc<AtomicBool>,
) -> DictationOutcome {
    let spec = match StreamSpec::new(Source::Microphone, SAMPLE_RATE, 1) {
        Ok(spec) => spec,
        Err(error) => return failed(error.into()),
    };
    let audio = match AudioSpec::new(SAMPLE_RATE, 1) {
        Ok(audio) => audio,
        Err(error) => return failed(DesktopError::Transcribe(error)),
    };
    let mut stream = match koe_capture::open(&spec) {
        Ok(stream) => stream,
        Err(error) => return failed(error.into()),
    };
    let options = plan
        .language
        .clone()
        .map_or_else(Options::new, |language| Options::new().language(language));
    let (sender, ready) = mpsc::channel::<Result<LiveSession, DesktopError>>();
    let model = plan.model.clone();
    let loader_stop = Arc::clone(stop);
    let spawned = thread::Builder::new()
        .name("koe-dictation-model".to_owned())
        .spawn(move || {
            // The lock serializes loads; the flag drops a load whose take
            // already ended while it waited, so nothing stacks.
            let _guard = MODEL_LOAD.lock().unwrap_or_else(PoisonError::into_inner);
            if loader_stop.load(Ordering::Acquire) {
                return;
            }
            let session = Transcriber::new(&model)
                .and_then(|transcriber| transcriber.live(audio, &options))
                .map_err(DesktopError::Transcribe);
            // A send that fails means the take ended; the session drops here.
            let _ = sender.send(session);
        });
    if let Err(error) = spawned {
        return failed(DesktopError::Thread(error.to_string()));
    }
    let (session, capture_failure) = pump(&mut stream, &ready, stop);
    finish(stream, session, capture_failure, cancel)
}

/// Reads the microphone until the take stops or the stream fails, feeding the
/// transcription session as soon as the model thread hands it over.
fn pump(
    stream: &mut koe_capture::CaptureStream,
    ready: &mpsc::Receiver<Result<LiveSession, DesktopError>>,
    stop: &Arc<AtomicBool>,
) -> (Option<LiveSession>, Option<DesktopError>) {
    let mut backlog: Vec<f32> = Vec::new();
    let mut session: Option<LiveSession> = None;
    let mut buffer = vec![0.0_f32; BLOCK_FRAMES];
    let mut capture_failure: Option<DesktopError> = None;
    loop {
        if stop.load(Ordering::Acquire) {
            break;
        }
        if session.is_none() {
            match ready.try_recv() {
                Ok(Ok(live)) => {
                    session = Some(live);
                    if session
                        .as_ref()
                        .is_some_and(|live| live.feed(&backlog).is_err())
                    {
                        // The session latched its failure; `finish` reports it.
                        break;
                    }
                    backlog = Vec::new();
                }
                Ok(Err(error)) => return (None, Some(error)),
                Err(TryRecvError::Empty) => {}
                Err(TryRecvError::Disconnected) => {
                    return (
                        None,
                        Some(DesktopError::Thread(
                            "the model thread ended without a session".to_owned(),
                        )),
                    );
                }
            }
        }
        match stream.read(&mut buffer) {
            // `CaptureStream::read` never returns Ok without a frame: a
            // closed stream is Err, so this arm owns every real read.
            Ok(frames) => {
                let samples = &buffer[..frames];
                match &session {
                    Some(live) => {
                        if live.feed(samples).is_err() {
                            break;
                        }
                    }
                    None => push_bounded(&mut backlog, samples),
                }
            }
            Err(error) => {
                capture_failure = Some(error.into());
                break;
            }
        }
    }
    (session, capture_failure)
}

/// Ends the session, inserts the transcript, and reports the outcome.
///
/// A cancelled take inserts nothing and reports nothing to show. A cancel
/// that arrives before the inference skips waiting for it; one that arrives
/// while it runs cannot stop the inference, but the check before the
/// insertion still discards its result.
fn finish(
    stream: koe_capture::CaptureStream,
    session: Option<LiveSession>,
    capture_failure: Option<DesktopError>,
    cancel: &Arc<AtomicBool>,
) -> DictationOutcome {
    drop(stream);
    if cancel.load(Ordering::Acquire) {
        return DictationOutcome {
            text: String::new(),
            failure: None,
        };
    }
    let mut text = String::new();
    let mut failure = capture_failure;
    match session {
        Some(live) => match live.finish() {
            Ok(transcript) => text = dictated_text(transcript.text()),
            Err(incomplete) => {
                text = dictated_text(incomplete.transcript().text());
                if failure.is_none() {
                    failure = Some(DesktopError::Transcribe(incomplete.error().clone()));
                }
            }
        },
        None if failure.is_none() => {
            failure = Some(DesktopError::Dictation(
                "stopped before the transcription model finished loading".to_owned(),
            ));
        }
        None => {}
    }
    // The insertion is the commit point, so a cancel that wins the race
    // against the inference still discards.
    if cancel.load(Ordering::Acquire) {
        return DictationOutcome {
            text: String::new(),
            failure: None,
        };
    }
    if !text.is_empty()
        && let Err(error) = type_text(&text)
    {
        failure = Some(error);
    }
    DictationOutcome { text, failure }
}

/// An outcome that captured nothing and inserted nothing.
const fn failed(failure: DesktopError) -> DictationOutcome {
    DictationOutcome {
        text: String::new(),
        failure: Some(failure),
    }
}

/// The text a transcript contributes to the focused app.
///
/// Transcription pads segments with whitespace, which would add stray
/// spaces around the dictated words.
#[must_use]
pub fn dictated_text(transcript: &str) -> String {
    transcript.trim().to_owned()
}

/// Appends `samples`, dropping the oldest once the backlog is full.
fn push_bounded(backlog: &mut Vec<f32>, samples: &[f32]) {
    if backlog.len() + samples.len() > BACKLOG_LIMIT {
        let overflow = (backlog.len() + samples.len() - BACKLOG_LIMIT).min(backlog.len());
        backlog.drain(..overflow);
    }
    backlog.extend_from_slice(samples);
}

/// Inserts `text` into whatever currently holds keyboard focus.
///
/// On Linux the text is pasted through the clipboard when one is available
/// and typed as Unicode otherwise. The paste is the primary path because a
/// synthetic keysym is indistinguishable from a physical key, so a running
/// input method (fcitx, ibus — the norm on a Japanese desktop) filters it
/// like typing, while a paste inserts the text without crossing the input
/// method at all. macOS and Windows type Unicode key events, which no input
/// method helps produce; whether a running one filters them was not
/// verified here.
///
/// # Errors
///
/// Returns [`DesktopError::Dictation`] when no input path accepts the text,
/// which happens without an X server on Linux, without the accessibility
/// permission on macOS, and when the focused window refuses synthetic input.
fn type_text(text: &str) -> Result<(), DesktopError> {
    #[cfg(target_os = "linux")]
    if paste_text(text) {
        return Ok(());
    }
    type_unicode(text)
}

/// Puts `text` on the clipboard, pastes it, and restores what was there.
///
/// What was there — text, or an image — comes back when the clipboard still
/// holds the transcript when the restore is due: anything the user copied
/// meanwhile stays. A clipboard holding more than [`SNAPSHOT_LIMIT`] bytes
/// is never touched; the caller types instead, so its content survives.
/// Once the paste shortcut is out, the remaining steps cannot fail the take;
/// a restore that does not land leaves the transcript on the clipboard,
/// where the user can still paste it.
///
/// # Errors
///
/// Returns [`false`] when the clipboard or the paste shortcut is
/// unavailable, or the clipboard holds more than [`SNAPSHOT_LIMIT`] bytes,
/// so the caller can fall back to typing. A shortcut that fails after the
/// clipboard was set returns [`false`] with the transcript left on the
/// clipboard, as hex does: the user's speech is worth more than the
/// clipboard it replaced.
#[allow(
    clippy::significant_drop_tightening,
    reason = "the guard must outlive the `&mut Clipboard` borrowed from it; the lint's suggested early drop does not compile"
)]
#[cfg(target_os = "linux")]
fn paste_text(text: &str) -> bool {
    let mut shared = CLIPBOARD.lock().unwrap_or_else(PoisonError::into_inner);
    if shared.is_none() {
        // A construction failure is not latched: the next paste asks again,
        // so a clipboard that appears after koe started still gets pastes.
        *shared = arboard::Clipboard::new().ok();
    }
    let Some(clipboard) = shared.as_mut() else {
        return false;
    };
    let Some(snapshot) = snapshot(clipboard) else {
        return false;
    };
    // The keyboard is proven before the clipboard is touched, so a dead X
    // server or an unusable keymap cannot cost the user their clipboard.
    if paste_keyboard().is_err() {
        return false;
    }
    if clipboard.set_text(text.to_owned()).is_err() {
        return false;
    }
    thread::sleep(PASTE_SETTLE);
    if send_paste_shortcut().is_err() {
        return false;
    }
    thread::sleep(RESTORE_DELAY);
    if clipboard.get_text().ok().as_deref() == Some(text) {
        restore(clipboard, snapshot);
    }
    true
}

/// The clipboard koe holds for its whole life.
///
/// An X selection lives only as long as its owner window: arboard hands its
/// contents to a clipboard manager when the last handle drops, and without a
/// manager (dwm, i3, `XMonad` — most tiling setups) the clipboard simply
/// dies. One handle held for the process keeps arboard's serve thread and
/// whatever the last paste put there alive until koe exits; a static never
/// drops, so the handover never runs.
#[cfg(target_os = "linux")]
static CLIPBOARD: Mutex<Option<arboard::Clipboard>> = Mutex::new(None);

/// What the clipboard held before a paste.
#[cfg(target_os = "linux")]
enum Snapshot {
    /// Nothing, or nothing readable: the transcript stays where the paste
    /// left it.
    Nothing,
    /// Text, which comes back after the paste.
    Text(String),
    /// An image, which comes back after the paste.
    Image(arboard::ImageData<'static>),
}

/// hex's cap on what a paste will hold for the restore: a bigger clipboard —
/// a huge image, a wall of text — is not worth the memory the dictation
/// would pin for it.
#[cfg(target_os = "linux")]
const SNAPSHOT_LIMIT: usize = 32 * 1024 * 1024;

/// Reads what the clipboard holds, then decides whether the paste can hold
/// it for the restore.
#[cfg(target_os = "linux")]
fn snapshot(clipboard: &mut arboard::Clipboard) -> Option<Snapshot> {
    let text = clipboard.get_text().ok();
    // The image is read only when there is no text, so a clipboard holding
    // both never pays for the picture it will not keep.
    let image = if text.is_none() {
        clipboard.get_image().ok()
    } else {
        None
    };
    bounded_snapshot(text, image)
}

/// Chooses what a paste will hold for the restore, from what the clipboard
/// read: the text over an image, and nothing when the clipboard held neither.
///
/// # Errors
///
/// Returns [`None`] when the content is over [`SNAPSHOT_LIMIT`] bytes, so the
/// caller leaves the clipboard untouched and types instead.
#[cfg(target_os = "linux")]
fn bounded_snapshot(
    text: Option<String>,
    image: Option<arboard::ImageData<'static>>,
) -> Option<Snapshot> {
    match (text, image) {
        (Some(text), _) if text.len() <= SNAPSHOT_LIMIT => Some(Snapshot::Text(text)),
        (None, Some(image)) if image.bytes.as_ref().len() <= SNAPSHOT_LIMIT => {
            Some(Snapshot::Image(image))
        }
        (None, None) => Some(Snapshot::Nothing),
        (Some(_), _) | (None, Some(_)) => None,
    }
}

/// Puts `snapshot` back. A restore that fails leaves the transcript on the
/// clipboard for a manual paste.
#[cfg(target_os = "linux")]
fn restore(clipboard: &mut arboard::Clipboard, snapshot: Snapshot) {
    let restored = match snapshot {
        Snapshot::Nothing => Ok(()),
        Snapshot::Text(text) => clipboard.set_text(text),
        Snapshot::Image(image) => clipboard.set_image(image),
    };
    let _ = restored;
}

/// Proves a synthetic keyboard can be built for the paste shortcut.
///
/// # Errors
///
/// Returns the constructor error when no keyboard exists — the same failure
/// the typing fallback would hit.
#[cfg(target_os = "linux")]
fn paste_keyboard() -> Result<(), String> {
    use enigo::{Enigo, Settings};
    Enigo::new(&Settings::default())
        .map(|_| ())
        .map_err(|error| error.to_string())
}

/// Sends Ctrl+V through the synthetic keyboard.
///
/// # Errors
///
/// Returns the connection or key error when the shortcut cannot be sent.
#[cfg(target_os = "linux")]
fn send_paste_shortcut() -> Result<(), String> {
    use enigo::{
        Direction::{Click, Press, Release},
        Enigo, Key, Keyboard, Settings,
    };
    let mut keyboard = Enigo::new(&Settings::default()).map_err(|error| error.to_string())?;
    keyboard
        .key(Key::Control, Press)
        .and_then(|()| keyboard.key(Key::Unicode('v'), Click))
        .and_then(|()| keyboard.key(Key::Control, Release))
        .map_err(|error| error.to_string())
}

/// Types `text` as Unicode key events.
///
/// # Errors
///
/// Returns [`DesktopError::Dictation`] when no keyboard accepts the events.
fn type_unicode(text: &str) -> Result<(), DesktopError> {
    use enigo::{Enigo, Keyboard, Settings};
    let mut keyboard = Enigo::new(&Settings::default()).map_err(|error| {
        DesktopError::Dictation(format!("connecting to the keyboard failed: {error}"))
    })?;
    keyboard
        .text(text)
        .map_err(|error| DesktopError::Dictation(format!("typing failed: {error}")))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::float_cmp, clippy::cast_precision_loss)]

    use super::*;

    #[test]
    fn dictated_text_drops_transcription_padding() {
        assert_eq!(dictated_text("  こんにちは  "), "こんにちは");
        assert_eq!(dictated_text(" hello world \n"), "hello world");
        assert_eq!(dictated_text(""), "");
        assert_eq!(dictated_text("   "), "");
    }

    #[test]
    fn the_backlog_keeps_every_sample_until_it_is_full() {
        let mut backlog = Vec::new();
        push_bounded(&mut backlog, &[0.1, 0.2]);
        push_bounded(&mut backlog, &[0.3]);
        assert_eq!(backlog, [0.1, 0.2, 0.3]);
    }

    #[test]
    fn a_full_backlog_drops_the_oldest_samples_first() {
        let mut backlog: Vec<f32> = (0..BACKLOG_LIMIT)
            .map(|sample| sample as f32 / 10.0)
            .collect();
        push_bounded(&mut backlog, &[7.0, 8.0, 9.0]);
        assert_eq!(backlog.len(), BACKLOG_LIMIT);
        assert_eq!(backlog[..3].to_vec(), vec![0.3, 0.4, 0.5]);
        assert_eq!(backlog[BACKLOG_LIMIT - 3..].to_vec(), vec![7.0, 8.0, 9.0]);
    }

    #[test]
    fn the_backlog_ceiling_is_one_minute_of_mono_audio() {
        assert_eq!(BACKLOG_LIMIT, 48_000 * 60);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn the_snapshot_cap_is_hexs_thirty_two_megabytes() {
        assert_eq!(SNAPSHOT_LIMIT, 32 * 1024 * 1024);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn a_text_clipboard_is_held_up_to_the_cap_and_refused_over_it() {
        let at = "x".repeat(SNAPSHOT_LIMIT);
        assert!(matches!(
            bounded_snapshot(Some(at), None),
            Some(Snapshot::Text(_))
        ));
        let over = "x".repeat(SNAPSHOT_LIMIT + 1);
        assert!(bounded_snapshot(Some(over), None).is_none());
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn an_image_clipboard_is_held_up_to_the_cap_and_refused_over_it() {
        let image = |bytes: usize| arboard::ImageData {
            width: 1,
            height: 1,
            bytes: std::borrow::Cow::Owned(vec![0; bytes]),
        };
        assert!(matches!(
            bounded_snapshot(None, Some(image(SNAPSHOT_LIMIT))),
            Some(Snapshot::Image(_))
        ));
        assert!(bounded_snapshot(None, Some(image(SNAPSHOT_LIMIT + 1))).is_none());
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn a_clipboard_with_nothing_readable_snapshots_nothing() {
        assert!(matches!(
            bounded_snapshot(None, None),
            Some(Snapshot::Nothing)
        ));
    }

    #[test]
    fn push_bounded_keeps_a_full_backlog_at_the_limit() {
        let mut backlog = vec![0.0_f32; BACKLOG_LIMIT - 1];
        push_bounded(&mut backlog, &[7.0]);
        assert_eq!(backlog.len(), BACKLOG_LIMIT);
        assert_eq!(backlog[BACKLOG_LIMIT - 1], 7.0);

        push_bounded(&mut backlog, &[8.0]);
        assert_eq!(backlog.len(), BACKLOG_LIMIT);
        assert_eq!(backlog[0], 0.0);
        assert_eq!(backlog[BACKLOG_LIMIT - 1], 8.0);
    }

    #[test]
    fn push_bounded_drops_the_oldest_once_full() {
        let half = BACKLOG_LIMIT / 2;
        let mut backlog = vec![0.0_f32; half];
        push_bounded(&mut backlog, &vec![1.0; half]);
        assert_eq!(backlog.len(), BACKLOG_LIMIT);

        push_bounded(&mut backlog, &vec![2.0; half]);
        assert_eq!(backlog.len(), BACKLOG_LIMIT);
        assert_eq!(backlog[0], 1.0);
        assert_eq!(backlog[BACKLOG_LIMIT - 1], 2.0);
        assert!(
            backlog.iter().all(|&sample| sample > 0.0),
            "the zeros the overflow dropped are gone"
        );
    }
}
