//! Everything that can happen to a session, and everything a session asks for.

use koe_capture_core::Source;

use crate::sources::SourceSet;
use crate::transcript::SegmentResult;

/// An event the application feeds to [`Session::step`][crate::Session::step].
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Input {
    /// Ask the operating system for a source's permission.
    RequestPermission(Source),
    /// The operating system granted a source's permission.
    PermissionGranted(Source),
    /// The user refused a source's permission.
    PermissionDenied(Source),
    /// The operating system withdrew a source's permission.
    PermissionRevoked(Source),
    /// Begin a meeting that records `sources`.
    Start(SourceSet),
    /// A capture device opened for a source.
    DeviceOpened(Source),
    /// A capture device could not be opened.
    DeviceFailed(Source),
    /// The user paused the meeting.
    Pause,
    /// The user resumed the meeting.
    Resume,
    /// The user stopped the meeting.
    Stop,
    /// A capture stream failed.
    CaptureFailed(Source),
    /// A transcription engine failed.
    EngineFailed(Source),
    /// A transcription engine committed text.
    EngineCommitted(Source),
    /// A transcription engine produced text it may still replace.
    EngineVolatile(Source),
    /// A transcription session finished its last segment.
    SegmentClosed {
        /// The source whose session closed.
        source: Source,
        /// What the session reported.
        result: SegmentResult,
    },
    /// The user accepted the transcript.
    Confirm,
    /// The user threw the meeting away.
    Reset,
}

/// Work the caller must do for a step to take effect.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Effect {
    /// Ask the operating system for a source's permission.
    RequestPermission(Source),
    /// Open capture for a source and start its transcription session.
    ///
    /// The caller must not feed audio until the session reaches
    /// [`Stage::Recording`][crate::Stage::Recording]. Preparation can open a
    /// stream before every chosen device is ready.
    OpenSource(Source),
    /// Close capture for a source. Repeating it is harmless.
    CloseSource(Source),
    /// Finish a source's transcription session and keep its transcript.
    FinishSource(Source),
    /// Drop a transcription session that never received audio.
    DiscardSource(Source),
    /// Persist the transcript.
    SaveTranscript,
    /// Throw the transcript away.
    DiscardTranscript,
}
