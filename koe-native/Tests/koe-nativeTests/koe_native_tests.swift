import KoeFfi
import XCTest

@testable import koe_native

final class KoeNativeTests: XCTestCase {
  override func setUp() {
    super.setUp()
    KoeFfiBootstrap.install()
  }

  func testKoeFfiAdd() {
    XCTAssertEqual(add(left: 2, right: 2), 4)
  }

  func testFfiCheckPermission() {
    let status = checkPermission(permission: .microphone)
    XCTAssertEqual(status, .notDetermined)
  }

  func testFfiEnumerateApps() {
    let apps = enumerateApps()
    XCTAssertEqual(
      apps,
      ProcessEnumerator.enumerateApps().map { app in
        AppInfo(
          pid: app.pid,
          name: app.name,
          bundleId: app.bundleID,
          hasAudio: app.hasAudio
        )
      })
  }

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
    do {
      let analyzer = try SpeechAnalyzerBridge(locale: "en-US")
      XCTAssertNotNil(analyzer)
      XCTAssertFalse(SpeechAnalyzerBridge.supportedLocales.isEmpty)
    } catch let error as SpeechAnalyzerBridge.Error {
      guard case .recognizerUnavailable = error else {
        return XCTFail("unexpected error \(error)")
      }
    }
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

  func testCaptureErrorVariantsAreDistinguishable() {
    let errors: [CaptureError] = [
      .PermissionDenied("microphone"),
      .NoAudioSource(bundleId: "com.example.app"),
      .StreamError(msg: "underrun"),
      .Internal(msg: "boom"),
    ]

    XCTAssertEqual(errors.count, Set(errors.map { String(describing: $0) }).count)

    switch errors[0] {
    case .PermissionDenied(let msg):
      XCTAssertEqual(msg, "microphone")
    default:
      XCTFail("expected PermissionDenied")
    }
    switch errors[1] {
    case .NoAudioSource(let bundleId):
      XCTAssertEqual(bundleId, "com.example.app")
    default:
      XCTFail("expected NoAudioSource")
    }
    switch errors[2] {
    case .StreamError(let msg):
      XCTAssertEqual(msg, "underrun")
    default:
      XCTFail("expected StreamError")
    }
    switch errors[3] {
    case .Internal(let msg):
      XCTAssertEqual(msg, "boom")
    default:
      XCTFail("expected Internal")
    }
  }

  func testStartCaptureThrowsNoAudioSource() {
    XCTAssertThrowsError(
      try startCapture(
        source: .appAudio(bundleId: ""),
        callback: NoopAudioCallback()
      )
    ) { error in
      guard let captureError = error as? CaptureError,
        case .NoAudioSource = captureError
      else {
        return XCTFail("expected CaptureError.NoAudioSource, got \(error)")
      }
    }
  }

  func testStartTranscriptionThrowsUnsupportedLocale() {
    XCTAssertThrowsError(
      try startTranscription(locale: "", callback: NoopTranscriptionCallback())
    ) { error in
      guard let transcriptionError = error as? TranscriptionError,
        case .UnsupportedLocale = transcriptionError
      else {
        return XCTFail("expected TranscriptionError.UnsupportedLocale, got \(error)")
      }
    }
  }

  func testStartRecordingThrowsConfigError() {
    XCTAssertThrowsError(
      try startRecording(
        source: .microphone,
        outputPath: "",
        locale: "en-US",
        format: .ogg(quality: 0.5),
        enableAec: false,
        comfortNoise: false,
        progressCallback: NoopProgressCallback()
      )
    ) { error in
      guard let recordingError = error as? RecordingError,
        case .ConfigError = recordingError
      else {
        return XCTFail("expected RecordingError.ConfigError, got \(error)")
      }
    }
  }
}

private final class NoopAudioCallback: AudioCallback {
  func onAudio(pcm: [Float], timestampMs: UInt64) {}
}

private final class NoopTranscriptionCallback: TranscriptionCallback {
  func onSegment(segment: TranscriptionSegment) {}
  func onError(error: String) {}
}

private final class NoopProgressCallback: ProgressCallback {
  func onStatus(status: RecordingStatus) {}
  func onSegment(segment: TranscriptionSegment) {}
  func onError(error: String) {}
}
