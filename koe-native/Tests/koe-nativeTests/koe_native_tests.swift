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
