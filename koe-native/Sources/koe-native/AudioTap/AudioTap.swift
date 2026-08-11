import AudioToolbox
import CoreAudio
import Foundation

/// Captures system audio from a specific process using a CoreAudio process tap.
/// The tap is installed on the process's audio output and delivers Float32 PCM
/// frames via a callback.
public final class AudioTap: Sendable {
  /// Errors that can occur during tap lifecycle.
  public enum Error: Swift.Error {
    case processNotFound
    case tapCreationFailed(status: OSStatus)
    case alreadyRunning
    case notRunning
  }

  /// Status of the audio tap.
  public enum Status: Equatable {
    case idle
    case running
    case error
  }

  /// Callback type for receiving audio data.
  /// - Parameters:
  ///   - pcm: Interleaved Float32 PCM samples at 48 kHz stereo.
  ///   - timestamp: Monotonic timestamp in milliseconds for alignment.
  public typealias AudioCallback = @convention(c) (
    _ pcm: UnsafePointer<Float>,
    _ frameCount: Int,
    _ timestamp: UInt64
  ) -> Void

  /// Initializes an audio tap for the given process ID.
  public init(pid: Int32) {}

  /// Starts capturing audio from the target process.
  public func start(callback: @escaping AudioCallback) throws {}

  /// Stops the audio tap and releases resources.
  public func stop() throws {}

  /// The current status of the tap.
  public var status: Status { .idle }
}
