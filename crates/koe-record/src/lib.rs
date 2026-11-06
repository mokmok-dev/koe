//! Single-file recording of system audio and microphone.
//!
//! The crate is a facade over `koe-record-core` and an Opus encoder. It mixes
//! every registered capture stream into one file: the microphone and the system
//! audio arrive as separate blocking streams, and each pair of blocks is mixed
//! and written as one interleaved block.
//!
//! The file format is chosen from the path extension; `.ogg`, `.oga`, and
//! `.opus` all name Ogg with Opus audio, and `.wav` names 16-bit PCM WAV.
//!
//! ```no_run
//! use std::error::Error;
//! use std::sync::Mutex;
//!
//! use koe_capture::{Source, StreamSpec};
//! use koe_record::{Channels, Recorder, RecordingSpec};
//!
//! fn main() -> Result<(), Box<dyn Error>> {
//!     let recording = RecordingSpec::new(48_000, Channels::Stereo)?;
//!     let mut recorder = Recorder::create("take.ogg", recording)?;
//!     recorder.add_source(
//!         Source::Microphone,
//!         &StreamSpec::new(Source::Microphone, 48_000, 1)?,
//!     )?;
//!     recorder.add_source(
//!         Source::SystemAudio,
//!         &StreamSpec::new(Source::SystemAudio, 48_000, 2)?,
//!     )?;
//!     let recorder = Mutex::new(recorder);
//!     let shared = &recorder;
//!
//!     std::thread::scope(|scope| {
//!         for (source, channels) in [(Source::Microphone, 1), (Source::SystemAudio, 2)] {
//!             scope.spawn(move || -> Result<(), Box<dyn Error + Send + Sync>> {
//!                 let mut stream = koe_capture::open(&StreamSpec::new(source, 48_000, channels)?)?;
//!                 let mut samples = vec![0.0_f32; usize::from(channels) * 960];
//!                 loop {
//!                     let frames = match stream.read(&mut samples) {
//!                         Ok(frames) => frames,
//!                         Err(_) => break,
//!                     };
//!                     let mut recorder = shared.lock().unwrap_or_else(|error| error.into_inner());
//!                     recorder.write(source, &samples[..frames * usize::from(channels)])?;
//!                 }
//!                 Ok(())
//!             });
//!         }
//!     });
//!
//!     let recorder = recorder.into_inner().unwrap_or_else(|error| error.into_inner());
//!     let summary = recorder.finish()?;
//!     println!("recorded {} frames over {:?}", summary.frames, summary.duration);
//!     Ok(())
//! }
//! ```

mod file;
mod format;
mod recorder;

pub use format::Format;
pub use koe_capture_core::StreamSpec;
pub use koe_record_core::{Channels, Error, MAX_GAIN, RecordingSpec, Source};
pub use recorder::{Recorder, Summary};
