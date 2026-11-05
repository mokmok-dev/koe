//! Platform-independent audio mixing, Ogg Opus framing, and RIFF/WAVE framing.
//!
//! The crate is `no_std` with `alloc`. It has no platform code and no system
//! library, so the facade crate owns the file system and the Opus codec. Here
//! live the recording spec, the mixer that folds microphone and system audio
//! into one interleaved block, the Ogg page framing around Opus packets, and
//! the RIFF/WAVE framing around 16-bit PCM samples.
//!
//! ```
//! use koe_record_core::{Channels, Input, Mixer, RecordingSpec};
//!
//! let spec = RecordingSpec::new(48_000, Channels::Stereo).expect("valid spec");
//! let mixer = Mixer::new(spec.channels());
//! let mut out = vec![0.0_f32; usize::from(spec.channels().count()) * 2];
//! let report = mixer
//!     .mix(
//!         Some(Input {
//!             channels: Channels::Mono,
//!             samples: &[0.5, 0.25],
//!         }),
//!         None,
//!         &mut out,
//!     )
//!     .expect("aligned buffers");
//! assert_eq!(report.frames, 2);
//! assert_eq!(out, [0.5, 0.5, 0.25, 0.25]);
//! ```

#![cfg_attr(not(test), no_std)]

extern crate alloc;

mod error;
mod mixer;
mod ogg;
mod sink;
mod spec;
mod wav;

pub use error::Error;
pub use mixer::{Input, MAX_GAIN, MixReport, Mixer};
pub use ogg::{OggOpusConfig, OggOpusWriter};
pub use sink::{PatchSink, Sink};
pub use spec::{Channels, MAX_SAMPLE_RATE, MIN_SAMPLE_RATE, RecordingSpec};
pub use wav::WavWriter;

pub use koe_capture_core::Source;
