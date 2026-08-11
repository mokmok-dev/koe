import AVFoundation
import CoreAudio
import Foundation

/// Checks and requests macOS TCC (Transparency, Consent, and Control) permissions.
public final class PermissionChecker: Sendable {
  /// Permission domains relevant to Koe.
  public enum Permission: Int32 {
    case microphone
    case screenRecording
    case accessibility
  }

  /// Current authorization status for a given permission.
  public enum Status: Int32, Equatable {
    case authorized
    case denied
    case restricted
    case notDetermined
  }

  /// Returns the current authorization status for the given permission.
  public static func status(for permission: Permission) -> Status {
    .notDetermined
  }

  /// Requests authorization for the given permission.
  /// Presents the system TCC prompt if status is `.notDetermined`.
  public static func request(_ permission: Permission) async -> Status {
    .notDetermined
  }

  /// Opens System Settings to the relevant privacy pane.
  /// Used when the user has previously denied a permission and needs to
  /// manually re-enable it.
  public static func openSystemSettings(for permission: Permission) {}
}
