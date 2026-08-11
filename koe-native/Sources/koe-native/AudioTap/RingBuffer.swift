import Foundation

/// Lock-free single-producer-single-consumer (SPSC) ring buffer for PCM audio
/// samples. Sized for 4 × 20 ms chunks (~3200 samples at 48 kHz stereo Float32).
///
/// Internal helper — not part of the public C ABI surface.
final class RingBuffer {
  /// Buffer capacity in samples.
  private let capacity: Int

  /// Underlying storage.
  private let buffer: UnsafeMutablePointer<Float>

  /// Write position (producer).
  private var writeIndex: Int = 0

  /// Read position (consumer).
  private var readIndex: Int = 0

  /// Creates a ring buffer with the specified sample capacity.
  init(capacity: Int) {
    self.capacity = capacity
    buffer = UnsafeMutablePointer<Float>.allocate(capacity: capacity)
  }

  deinit {
    buffer.deallocate()
  }

  /// Writes samples from `source` into the buffer.
  /// Returns the number of samples actually written.
  func write(_ source: UnsafePointer<Float>, count: Int) -> Int {
    0
  }

  /// Reads samples from the buffer into `destination`.
  /// Returns the number of samples actually read.
  func read(into destination: UnsafeMutablePointer<Float>, maxCount: Int) -> Int {
    0
  }

  /// The number of samples available to read without blocking.
  var availableSamples: Int { 0 }
}
