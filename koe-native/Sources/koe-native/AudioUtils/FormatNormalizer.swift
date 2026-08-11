import Accelerate
import CoreAudio
import Foundation

/// Converts audio between different sample rates, channel layouts, and formats
/// using Accelerate framework routines.
///
/// Internal helper — not part of the public C ABI surface.
enum FormatNormalizer {
    /// Resamples interleaved Float32 PCM from `srcSampleRate` to `dstSampleRate`.
    /// - Parameters:
    ///   - input: Input buffer (interleaved).
    ///   - inputFrameCount: Number of frames in the input buffer.
    ///   - srcSampleRate: Source sample rate in Hz.
    ///   - dstSampleRate: Destination sample rate in Hz.
    ///   - channelCount: Number of channels.
    /// - Returns: Resampled output buffer, or nil on error.
    static func resample(
        input: UnsafePointer<Float>,
        inputFrameCount: Int,
        srcSampleRate: Double,
        dstSampleRate: Double,
        channelCount: Int
    ) -> [Float]? {
        nil
    }

    /// Converts a non-interleaved stereo buffer to interleaved.
    static func interleave(
        left: UnsafePointer<Float>,
        right: UnsafePointer<Float>,
        frameCount: Int
    ) -> [Float] {
        []
    }

    /// Converts an interleaved buffer to non-interleaved stereo (two separate buffers).
    static func deinterleave(
        interleaved: UnsafePointer<Float>,
        frameCount: Int
    ) -> (left: [Float], right: [Float]) {
        ([], [])
    }
}
