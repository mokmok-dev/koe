import Foundation
import ScreenCaptureKit

/// Captures audio from a specific application or display using ScreenCaptureKit.
public final class ScreenAudioCapture: Sendable {
  /// Errors that can occur during capture lifecycle.
  public enum Error: Swift.Error {
    case permissionDenied
    case noAudioContent
    case streamError(String)
    case alreadyCapturing
  }

  /// Callback type for receiving captured audio data.
  /// - Parameters:
  ///   - pcm: Interleaved Float32 PCM samples at 48 kHz stereo.
  ///   - frameCount: Number of frames in the buffer.
  ///   - timestamp: Monotonic timestamp in milliseconds.
  public typealias AudioCallback = @convention(c) (
    _ pcm: UnsafePointer<Float>,
    _ frameCount: Int,
    _ timestamp: UInt64
  ) -> Void

  /// Initializes a capture session for the given bundle identifier.
  /// - Parameter bundleID: The bundle identifier of the target application (e.g., "com.google.Chrome").
  public init(bundleID: String) {}

  /// Returns the list of shareable content (apps and displays with audio).
  public static func shareableContent() async throws -> [String] {
    []
  }

  /// Starts capturing audio from the target application.
  public func start(callback: @escaping AudioCallback) throws {}

  /// Stops the capture session and releases resources.
  public func stop() throws {}
}
