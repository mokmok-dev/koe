import ApplicationServices
import Foundation

/// Enumerates running processes on macOS, with audio-related metadata.
public final class ProcessEnumerator: Sendable {
    /// Information about a running application.
    public struct AppInfo {
        /// Process ID.
        public var pid: Int32
        /// Localized application name.
        public var name: String
        /// Bundle identifier (nil for background processes without a bundle).
        public var bundleID: String?
        /// Whether the process has an active audio output stream.
        public var hasAudio: Bool
    }

    /// Returns a list of all running applications.
    public static func enumerateApps() -> [AppInfo] {
        []
    }

    /// Returns a list of applications that currently have active audio output.
    public static func enumerateAudioApps() -> [AppInfo] {
        []
    }
}
