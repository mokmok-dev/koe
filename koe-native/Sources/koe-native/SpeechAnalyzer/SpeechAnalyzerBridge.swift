import AVFoundation
import Foundation
import Speech

/// Bridges macOS `SFSpeechAnalyzer` (on-device speech recognition) to the Rust
/// pipeline. Receives PCM audio chunks and emits transcription segments.
public final class SpeechAnalyzerBridge: Sendable {
    /// A single transcription segment produced by SFSpeechAnalyzer.
    public struct Segment {
        /// The recognized text.
        public var text: String
        /// Start time offset in milliseconds.
        public var startMs: Int64
        /// End time offset in milliseconds.
        public var endMs: Int64
        /// Whether this segment is final or may be updated.
        public var isFinal: Bool
    }

    /// Callback type for receiving transcription segments.
    public typealias SegmentCallback = @convention(c) (
        _ text: UnsafePointer<CChar>,
        _ startMs: Int64,
        _ endMs: Int64,
        _ isFinal: Bool
    ) -> Void

    /// Callback type for receiving error messages.
    public typealias ErrorCallback = @convention(c) (
        _ message: UnsafePointer<CChar>
    ) -> Void

    /// Initializes the speech analyzer for the given locale.
    /// - Parameter locale: BCP-47 locale identifier (e.g., "en-US", "ja-JP").
    public init(locale: String) throws {}

    /// Feeds a chunk of PCM audio data to the speech analyzer.
    /// - Parameters:
    ///   - pcm: Interleaved Float32 PCM samples at 48 kHz mono.
    ///   - frameCount: Number of frames in the buffer.
    public func feedAudio(_ pcm: UnsafePointer<Float>, frameCount: Int) {}

    /// Signals end-of-stream and finalizes pending transcription results.
    public func finalize() {}

    /// Sets the callback to receive transcription segments.
    public func onSegment(_ callback: @escaping SegmentCallback) {}

    /// Sets the callback to receive error messages.
    public func onError(_ callback: @escaping ErrorCallback) {}
}
