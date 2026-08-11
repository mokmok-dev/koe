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

  func testScreenAudioCaptureInitialization() {
    // Stream setup requires Screen Recording permission (interactive), so only
    // verify construction and the value type used to deliver chunks.
    let capture = ScreenAudioCapture()
    XCTAssertNotNil(capture)

    let buffer = ScreenAudioCapture.AudioBuffer(
      samples: [0.0, 0.0],
      frameCount: 1,
      timestampMilliseconds: 123
    )
    XCTAssertEqual(buffer.frameCount, 1)
    XCTAssertEqual(buffer.samples.count, 2)
    XCTAssertEqual(buffer.timestampMilliseconds, 123)
  }

  func testProcessEnumeratorEmpty() {
    let apps = ProcessEnumerator.enumerateApps()
    XCTAssertEqual(apps, [])
  }
}
