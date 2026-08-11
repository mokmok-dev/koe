import XCTest

@testable import koe_native

final class KoeNativeTests: XCTestCase {
  func testAudioTapRejectsUnknownProcess() {
    // PID 0 has no audio object; tap creation must fail cleanly.
    XCTAssertThrowsError(try AudioTap(pid: 0)) { error in
      guard case AudioTap.Error.processNotFound = error else {
        return XCTFail("expected processNotFound, got \(error)")
      }
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
