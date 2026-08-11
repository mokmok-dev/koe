import XCTest

@testable import koe_native

final class KoeNativeTests: XCTestCase {
  func testAudioTapInitialization() {
    let tap = AudioTap(pid: 1234)
    XCTAssertNotNil(tap)
    if case .idle = tap.status {
    } else {
      XCTFail("expected idle status")
    }
  }

  func testMicrophoneCaptureLifecycle() {
    // Requires microphone permission. In CI it is not granted (throws
    // permissionDenied); on dev machines with permission it creates and the
    // level starts silent.
    do {
      let capture = try MicrophoneCapture()
      XCTAssertNotNil(capture)
      XCTAssertEqual(capture.currentLevel, 0.0)
    } catch let error as MicrophoneCapture.Error {
      guard case .permissionDenied = error else {
        return XCTFail("unexpected error \(error)")
      }
    } catch {
      XCTFail("unexpected error \(error)")
    }
  }

  func testPermissionCheckerDefaults() {
    let micStatus = PermissionChecker.status(for: .microphone)
    XCTAssertEqual(micStatus, .notDetermined)
  }

  func testSpeechAnalyzerInitialization() throws {
    let analyzer = try SpeechAnalyzerBridge(locale: "en-US")
    XCTAssertNotNil(analyzer)
  }

  func testScreenCaptureInitialization() {
    let capture = ScreenAudioCapture(bundleID: "com.example.app")
    XCTAssertNotNil(capture)
  }

  func testProcessEnumeratorEmpty() {
    let apps = ProcessEnumerator.enumerateApps()
    XCTAssertEqual(apps, [])
  }
}
